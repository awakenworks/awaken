use std::fs;
use std::time::Duration;

use awaken_agent_config::{
    AuditedConfigWrite, ManagementAuditRecord, ScopeId, ScopedConfigRegistry,
};
use awaken_config_store::SqliteConfigStore;
use awaken_reliability_testkit::CrashProcess;

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
    let status = CrashProcess::new(
        "durable_audit_survives_process_kill_before_business_commit",
        &marker,
    )
    .env(CHILD_MODE, "1")
    .env(DB_PATH, &db)
    .env(MARKER_PATH, &marker)
    .run()
    .expect("child reaches the durable-audit boundary and is killed");
    assert!(!status.success());

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
