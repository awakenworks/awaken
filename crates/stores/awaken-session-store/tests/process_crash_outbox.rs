use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, ManagedSessionRepository, PersistedSession,
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
    let environment = awaken_session_contract::EnvironmentSnapshot {
        environment_id: "env".into(),
        revision: awaken_session_contract::env_registry::EnvironmentRevision(1),
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-1".into()),
        sandbox: serde_json::json!({}),
        sandbox_provisioning: Default::default(),
        packages: Default::default(),
        network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization: awaken_credential_contract::CredentialRealizationProfile {
            inference_holder: holder.clone(),
            mcp_holder: holder.clone(),
            resource_holder: holder,
        },
    };
    PersistedSession {
        session_id: "sesn_process_crash".into(),
        revision: Default::default(),
        baseline: awaken_session_contract::SessionBaselineState::Frozen(
            awaken_session_contract::SessionBaseline::compile(
                awaken_session_contract::SessionBaselineInputs {
                    environment,
                    mcp_authoring: Default::default(),
                    agent_id: "assistant".into(),
                    model: "model".into(),
                    runtime: None,
                    application: None,
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                },
            ),
        ),
        title: None,
        metadata: BTreeMap::new(),
        agent_tools: Vec::new(),
        environment_binding: None,
        mcp: Default::default(),
        resources: Default::default(),
        realization: None,
        status: "idle".into(),
        archived_at: None,
    }
}

fn fact() -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: "session:sesn_process_crash:created".into(),
        object_id: "sesn_process_crash".into(),
        workspace_id: Some("ws_a".into()),
        event_type: "session.status_idled".into(),
        timestamp: 1_700_000_000,
    }
}

#[tokio::test]
async fn session_commit_survives_process_kill_before_notification() {
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
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("session_commit_survives_process_kill_before_notification")
        .arg("--nocapture")
        .env(CHILD_MODE, "1")
        .env(DB_PATH, &db)
        .env(MARKER_PATH, &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "child did not reach the post-commit failpoint"
    );
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());

    let repo = SqliteManagedSessionRepository::open(db.to_str().unwrap()).unwrap();
    let mut expected = session();
    expected.revision = awaken_session_contract::SessionRevision(1);
    assert_eq!(repo.get("sesn_process_crash").await, Some(expected));
    assert_eq!(repo.pending_lifecycle().await, vec![fact()]);
    repo.complete_lifecycle(&fact().id).await;
    repo.complete_lifecycle(&fact().id).await;
    assert!(repo.pending_lifecycle().await.is_empty());
}
