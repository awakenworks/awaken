//! One backend conformance table for the domain-owned management audit identity.
use awaken_agent_config::{
    AgentConfig, AuditedConfigWrite, ManagementAuditRecord, ScopeId, ScopedConfigRegistry,
};

pub fn compact_history() -> awaken_scoped_migration::MigrationBundle {
    // Reuse the immutable published V1, not a second fixture schema definition.
    let current = awaken_config_store::config_bundle().unwrap();
    awaken_scoped_migration::MigrationBundle::new(
        current.bundle_id(),
        vec![current.migrations()[0].clone()],
    )
    .unwrap()
}

pub async fn decision_table(store: &dyn ScopedConfigRegistry, config: &AgentConfig) {
    // Cause/effect graph: tuple+scope select one intent; record equality permits
    // retry; pending permits one atomic write; committed returns replay. Rules:
    // I0 missing tuple => no read/mark/business write; I1 colon-colliding tuples
    // coexist; I2 changed content => reject; I3 exact pending => apply once;
    // I4 exact committed => no second write; I5 other tuple/scope unchanged.
    // Empty, Unicode and multi-colon coordinates remain legal (HTTP uses ':').
    let scope = ScopeId::from("audit-identity");
    let other = ScopeId::from("audit-identity-other");
    let tuples = [
        ("http:POST", "a:b"),
        ("http", "POST:a:b"),
        ("", ""),
        ("管理:工具", "调用:λ"),
        ("管理", "工具:调用:λ"),
    ];
    let records: Vec<_> = tuples
        .into_iter()
        .map(|(tool, call_id)| ManagementAuditRecord {
            tool: tool.into(),
            call_id: call_id.into(),
            summary: "body_sha256=one".into(),
        })
        .collect();
    store
        .record_management_audit_scoped(&scope, &records[0])
        .await
        .unwrap();
    let collision = &records[1];
    assert!(
        store
            .get_management_audit_scoped(&scope, &collision.tool, &collision.call_id)
            .await
            .unwrap()
            .is_none(),
        "I0 read"
    );
    assert!(
        store
            .mark_management_audit_committed_scoped(&scope, &collision.tool, &collision.call_id)
            .await
            .is_err(),
        "I0 mark"
    );
    assert!(
        store
            .put_config_with_audit_scoped(&scope, config, 0, collision)
            .await
            .is_err(),
        "I0 write"
    );
    assert!(
        store
            .get_config_revision_scoped(&scope, &config.id)
            .await
            .unwrap()
            .is_none()
    );
    for record in &records[1..] {
        assert_eq!(
            store
                .record_management_audit_scoped(&scope, record)
                .await
                .unwrap(),
            AuditedConfigWrite::Applied,
            "I1"
        );
    }
    store
        .record_management_audit_scoped(&other, &records[0])
        .await
        .unwrap();
    for record in &records {
        assert_eq!(
            store
                .record_management_audit_scoped(&scope, record)
                .await
                .unwrap(),
            AuditedConfigWrite::Replayed
        );
        let mut changed = record.clone();
        changed.summary.push('!');
        assert!(
            store
                .record_management_audit_scoped(&scope, &changed)
                .await
                .is_err(),
            "I2 begin"
        );
        assert!(
            store
                .put_config_with_audit_scoped(&scope, config, 0, &changed)
                .await
                .is_err(),
            "I2 commit"
        );
        let entry = store
            .get_management_audit_scoped(&scope, &record.tool, &record.call_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&entry.record, record);
        assert!(!entry.business_committed);
    }
    assert_eq!(
        store
            .put_config_with_audit_scoped(&scope, config, 0, &records[0])
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "I3"
    );
    let before = store
        .get_config_revision_scoped(&scope, &config.id)
        .await
        .unwrap()
        .unwrap();
    let mut retry = config.clone();
    retry.instructions.push_str("must not be written");
    assert_eq!(
        store
            .put_config_with_audit_scoped(&scope, &retry, 0, &records[0])
            .await
            .unwrap(),
        AuditedConfigWrite::Replayed,
        "I4"
    );
    let after = store
        .get_config_revision_scoped(&scope, &config.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.config, before.config);
    for record in &records[1..] {
        assert!(
            !store
                .get_management_audit_scoped(&scope, &record.tool, &record.call_id)
                .await
                .unwrap()
                .unwrap()
                .business_committed,
            "I5 tuple"
        );
    }
    assert!(
        !store
            .get_management_audit_scoped(&other, &records[0].tool, &records[0].call_id)
            .await
            .unwrap()
            .unwrap()
            .business_committed,
        "I5 scope"
    );
    store
        .mark_management_audit_committed_scoped(&scope, &collision.tool, &collision.call_id)
        .await
        .unwrap();
    assert!(
        store
            .get_management_audit_scoped(&scope, &collision.tool, &collision.call_id)
            .await
            .unwrap()
            .unwrap()
            .business_committed
    );
}

pub async fn concurrent_begin(
    left_store: &dyn ScopedConfigRegistry,
    right_store: &dyn ScopedConfigRegistry,
) {
    // Concurrent begin decision table:
    // | rule | same scoped call id | record content | outcomes |
    // | B1   | yes                 | exact same     | one Applied + one Replayed |
    // | B2   | yes                 | different      | one Applied + one conflict error |
    // The ON CONFLICT audit-begin port is the only INSERT owner; the final
    // config/effect transaction requires the winning pending row.
    let scope = ScopeId::from("audit-race");
    let exact = ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "same".into(),
        summary: "same request".into(),
    };
    let (left, right) = tokio::join!(
        left_store.record_management_audit_scoped(&scope, &exact),
        right_store.record_management_audit_scoped(&scope, &exact),
    );
    let mut exact_outcomes = vec![left.unwrap(), right.unwrap()];
    exact_outcomes.sort_by_key(|outcome| match outcome {
        AuditedConfigWrite::Applied => 0,
        AuditedConfigWrite::Replayed => 1,
        AuditedConfigWrite::Conflict { .. } => 2,
    });
    assert_eq!(
        exact_outcomes,
        vec![AuditedConfigWrite::Applied, AuditedConfigWrite::Replayed],
        "B1"
    );

    let first = ManagementAuditRecord {
        call_id: "different".into(),
        summary: "first request".into(),
        ..exact.clone()
    };
    let second = ManagementAuditRecord {
        summary: "second request".into(),
        ..first.clone()
    };
    let (left, right) = tokio::join!(
        left_store.record_management_audit_scoped(&scope, &first),
        right_store.record_management_audit_scoped(&scope, &second),
    );
    assert_eq!(
        usize::from(matches!(left, Ok(AuditedConfigWrite::Applied)))
            + usize::from(matches!(right, Ok(AuditedConfigWrite::Applied))),
        1,
        "B2 one winner"
    );
    assert_eq!(
        usize::from(left.is_err()) + usize::from(right.is_err()),
        1,
        "B2"
    );
}
