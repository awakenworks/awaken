// @formal evidence webhook.committed_outbox_row_is_recoverable
// @formal evidence webhook.lifecycle_commit_and_outbox_are_atomic
use std::collections::BTreeMap;
use std::time::Duration;

use awaken_reliability_testkit::CrashProcess;
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionExecutionState,
    SessionMutationPayload,
};
use awaken_session_store::SqliteManagedSessionRepository;

async fn repository(path: &str) -> Box<dyn awaken_session_application::SessionStore> {
    if let Some(pool) = awaken_reliability_testkit::postgres_crash_pool(path).await {
        return Box::new(
            awaken_session_store::PostgresManagedSessionRepository::with_pool(pool)
                .await
                .unwrap(),
        );
    }
    Box::new(SqliteManagedSessionRepository::open(path).await.unwrap())
}

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
    let mut legacy = PersistedSession::frozen_with_budget(
        "sesn_process_crash",
        awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
                environment,
                runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
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
        Default::default(),
        Default::default(),
        None,
        BTreeMap::new(),
        Default::default(),
        Default::default(),
    );
    legacy.execution = SessionExecutionState::Idle;
    // This fixture models the pre-cutover missing field, not a fresh creation.
    let mut wire = serde_json::to_value(legacy).unwrap();
    wire.as_object_mut()
        .unwrap()
        .remove("environment_materialization");
    serde_json::from_value(wire).unwrap()
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
        let repo = repository(&db).await;
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

    let repo = repository(db.to_str().unwrap()).await;
    let mut expected = session();
    expected.revision = awaken_session_types::SessionRevision(1);
    assert_eq!(
        repo.get(&awaken_session_types::SessionId::from("sesn_process_crash"))
            .await,
        Ok(expected)
    );
    assert_eq!(repo.pending_lifecycle().await.unwrap(), vec![fact()]);
    let committed_root = repo.get(&"sesn_process_crash".into()).await.unwrap();
    repo.complete_lifecycle(&fact().id).await.unwrap();
    // The completion response can be lost immediately before another process
    // takes over. Cold replay must observe the acknowledged durable fact and
    // cannot advance the root revision or enqueue a replacement notification.
    drop(repo);
    let recovered = repository(&dir.path().join("sessions.db").to_string_lossy()).await;
    assert!(recovered.pending_lifecycle().await.unwrap().is_empty());
    recovered.complete_lifecycle(&fact().id).await.unwrap();
    assert!(recovered.pending_lifecycle().await.unwrap().is_empty());
    assert_eq!(
        recovered.get(&"sesn_process_crash".into()).await.unwrap(),
        committed_root
    );
}
