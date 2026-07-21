use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use awaken_session_contract::{ManagedSessionRepository, PersistedSession, SessionLifecycleFact};
use awaken_session_store::SqliteManagedSessionRepository;

const CHILD_MODE: &str = "AWAKEN_SESSION_CRASH_CHILD";
const DB_PATH: &str = "AWAKEN_SESSION_CRASH_DB";
const MARKER_PATH: &str = "AWAKEN_SESSION_CRASH_MARKER";

fn session() -> PersistedSession {
    PersistedSession {
        session_id: "sesn_process_crash".into(),
        agent_id: "assistant".into(),
        model: "model".into(),
        title: None,
        metadata: BTreeMap::new(),
        environment_id: "env".into(),
        mcp_servers: Vec::new(),
        effective_inputs: Default::default(),
        status: "idle".into(),
        archived_at: None,
    }
}

fn fact() -> SessionLifecycleFact {
    SessionLifecycleFact {
        id: "session:sesn_process_crash:created".into(),
        session_id: "sesn_process_crash".into(),
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
        repo.save_owned_with_lifecycle("ws_a", session(), fact())
            .await;
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
    assert_eq!(repo.get("sesn_process_crash").await, Some(session()));
    assert_eq!(repo.pending_lifecycle().await, vec![fact()]);
    repo.complete_lifecycle(&fact().id).await;
    repo.complete_lifecycle(&fact().id).await;
    assert!(repo.pending_lifecycle().await.is_empty());
}
