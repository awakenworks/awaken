use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use awaken_config_store::{
    AuditedConfigWrite, ManagementAuditRecord, ScopeId, ScopedConfigRegistry, SqliteConfigStore,
};

const CHILD_MODE: &str = "AWAKEN_AUDIT_CRASH_CHILD";
const DB_PATH: &str = "AWAKEN_AUDIT_CRASH_DB";
const MARKER_PATH: &str = "AWAKEN_AUDIT_CRASH_MARKER";

#[tokio::test]
async fn durable_audit_survives_process_kill_before_business_commit() {
    if std::env::var_os(CHILD_MODE).is_some() {
        let store = SqliteConfigStore::open(&std::env::var(DB_PATH).unwrap()).unwrap();
        assert_eq!(
            store
                .record_management_audit_scoped(&ScopeId::from("ws"), &audit())
                .await
                .unwrap(),
            AuditedConfigWrite::Applied
        );
        fs::write(std::env::var(MARKER_PATH).unwrap(), b"audit-durable").unwrap();
        std::thread::sleep(Duration::from_secs(30));
        return;
    }

    let root = std::env::temp_dir().join(format!("awaken-audit-crash-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let db = root.join("config.db");
    let marker = root.join("marker");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("durable_audit_survives_process_kill_before_business_commit")
        .arg("--exact")
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
        "child did not reach the post-audit failpoint"
    );
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());

    let store = SqliteConfigStore::open(db.to_str().unwrap()).unwrap();
    let entry = store
        .get_management_audit_scoped(&ScopeId::from("ws"), "http:POST:/v1/mutate", "request-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.record, audit());
    assert!(!entry.business_committed);
    assert_eq!(
        store
            .record_management_audit_scoped(&ScopeId::from("ws"), &audit())
            .await
            .unwrap(),
        AuditedConfigWrite::Replayed
    );
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

fn audit() -> ManagementAuditRecord {
    ManagementAuditRecord {
        tool: "http:POST:/v1/mutate".into(),
        call_id: "request-1".into(),
        summary: "body_sha256=stable".into(),
    }
}
