use std::time::Duration;

use awaken_agent_config::{
    AgentConfig, AgentInputConfig, AuditedConfigWrite, ManagementAuditRecord, ManagementEffect,
    ScopeId, ScopedConfigRegistry,
};
use awaken_config_store::SqliteConfigStore;
use awaken_reliability_testkit::CrashProcess;

const CHILD_MODE: &str = "AWAKEN_AUDIT_CRASH_CHILD";
const DB_PATH: &str = "AWAKEN_AUDIT_CRASH_DB";
const MARKER_PATH: &str = "AWAKEN_AUDIT_CRASH_MARKER";

#[tokio::test]
async fn durable_audit_survives_process_kill_before_business_commit() {
    // Causes: kill after audit intent (D1) or after atomic config/audit/effect
    // commit but before the caller receives a response (D2). Effects: reopen
    // sees either pending+no config/effect or committed+revision 1+one effect.
    // D3 exact retry applies once/replays; D4 changed fingerprint is rejected;
    // D5 the same identity in another scope remains independently pending.
    // CrashProcess kills a separate process; no destructor or graceful flush
    // is used as a substitute for crash durability.
    let scope = ScopeId::from("ws");
    let other = ScopeId::from("other");
    let config = AgentConfig {
        id: "crash-agent".into(),
        instructions: "durable configuration".into(),
        ..Default::default()
    };
    let effect = ManagementEffect::UpsertAgentInputs {
        config: AgentInputConfig {
            agent_id: config.id.clone(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        },
    };
    if let Ok(phase) = std::env::var(CHILD_MODE) {
        let store = SqliteConfigStore::open(&std::env::var(DB_PATH).unwrap()).unwrap();
        for target in [&scope, &other] {
            assert_eq!(
                store
                    .record_management_audit_scoped(target, &audit())
                    .await
                    .unwrap(),
                AuditedConfigWrite::Applied
            );
        }
        if phase == "committed" {
            assert_eq!(
                store
                    .put_config_with_audit_effect_scoped(
                        &scope,
                        &config,
                        0,
                        &audit(),
                        Some(&effect)
                    )
                    .await
                    .unwrap(),
                AuditedConfigWrite::Applied
            );
        }
        std::fs::write(std::env::var(MARKER_PATH).unwrap(), b"durable-boundary").unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
        panic!("parent failed to kill crash child");
    }

    for committed in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "awaken-audit-crash-{}-{committed}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("config.db");
        let marker = root.join("marker");
        let status = CrashProcess::new(
            "durable_audit_survives_process_kill_before_business_commit",
            &marker,
        )
        .env(CHILD_MODE, if committed { "committed" } else { "pending" })
        .env(DB_PATH, &db)
        .env(MARKER_PATH, &marker)
        .run()
        .expect("child reaches the durable boundary and is killed");
        assert!(!status.success());
        let store = SqliteConfigStore::open(db.to_str().unwrap()).unwrap();
        let entry = store
            .get_management_audit_scoped(&scope, &audit().tool, &audit().call_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.record, audit());
        assert_eq!(entry.business_committed, committed, "D1/D2 audit");
        let revision = store
            .get_config_revision_scoped(&scope, &config.id)
            .await
            .unwrap();
        assert_eq!(
            revision.as_ref().map(|r| r.revision),
            committed.then_some(1)
        );
        assert_eq!(
            store
                .pending_management_effects_scoped(&scope)
                .await
                .unwrap(),
            if committed {
                vec![effect.clone()]
            } else {
                vec![]
            }
        );
        let mut changed = audit();
        changed.summary.push_str("-different-payload");
        assert!(
            store
                .record_management_audit_scoped(&scope, &changed)
                .await
                .is_err(),
            "D4 begin"
        );
        assert!(
            store
                .put_config_with_audit_effect_scoped(&scope, &config, 0, &changed, Some(&effect))
                .await
                .is_err(),
            "D4 commit"
        );
        for retry in 0..2 {
            assert_eq!(
                store
                    .record_management_audit_scoped(&scope, &audit())
                    .await
                    .unwrap(),
                AuditedConfigWrite::Replayed
            );
            assert_eq!(
                store
                    .put_config_with_audit_effect_scoped(
                        &scope,
                        &config,
                        0,
                        &audit(),
                        Some(&effect)
                    )
                    .await
                    .unwrap(),
                if committed || retry > 0 {
                    AuditedConfigWrite::Replayed
                } else {
                    AuditedConfigWrite::Applied
                },
                "D3"
            );
        }
        let revision = store
            .get_config_revision_scoped(&scope, &config.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(revision.revision, 1);
        assert_eq!(revision.config, config);
        assert_eq!(
            store
                .pending_management_effects_scoped(&scope)
                .await
                .unwrap(),
            vec![effect.clone()]
        );
        assert!(
            !store
                .get_management_audit_scoped(&other, &audit().tool, &audit().call_id)
                .await
                .unwrap()
                .unwrap()
                .business_committed,
            "D5 audit"
        );
        assert!(
            store
                .get_config_revision_scoped(&other, &config.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .pending_management_effects_scoped(&other)
                .await
                .unwrap()
                .is_empty()
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn audit() -> ManagementAuditRecord {
    ManagementAuditRecord {
        tool: "http:POST:/v1/mutate".into(),
        call_id: "request-1".into(),
        summary: "body_sha256=stable".into(),
    }
}
