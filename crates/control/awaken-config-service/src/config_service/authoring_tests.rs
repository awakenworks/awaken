#[tokio::test]
async fn mutable_admission_revision_is_the_only_cas_revision() {
    // Cause/effect graph: C1 admission observes Published r1; C2 archive wins
    // and stores Archived r2 before the authoring CAS; C3 a caller supplies
    // either r1 (ordinary) or the future r2 (explicit CAS). E1 neither path
    // may use a revision other than the one admission actually observed;
    // E2 Archived r2 remains authoritative.
    //
    // Decision table:
    // | rule | API | caller expected | observed | current at CAS | result |
    // | T1 | ordinary | derived r1 | r1 | archived r2 | conflict/error |
    // | T2 | CAS | future r2 | r1 | archived r2 | Conflict(r1), no CAS |
    let service = test_service();
    let source = agent_config("interleaved");
    let mut candidate = source.clone();
    candidate.instructions = "late authoring must not revive".into();

    let ordinary = ArchiveBetweenAdmissionAndCas::new(source.clone());
    let error = service.put(&ordinary, &candidate).await.expect_err("T1");
    assert!(error.contains("changed concurrently"), "T1/E1: {error}");
    let current = ordinary.current.lock().unwrap().clone();
    assert_eq!(current.revision, 2, "T1/E2");
    assert!(current.config.archived_at.is_some(), "T1/E2");

    let explicit = ArchiveBetweenAdmissionAndCas::new(source);
    assert_eq!(
        service
            .put_if_revision(&explicit, &candidate, 2)
            .await
            .expect("T2"),
        ConfigWrite::Conflict {
            current_revision: Some(1)
        },
        "T2/E1"
    );
    let current = explicit.current.lock().unwrap().clone();
    assert_eq!(current.revision, 2, "T2/E2");
    assert!(current.config.archived_at.is_some(), "T2/E2");
}

#[tokio::test]
async fn mutable_writes_reject_legacy_permission_authoring_without_a_partial_commit() {
    // Cause/effect graph: C1 ordinary upsert, C2 revision-fenced CAS, and C3
    // audited write all receive the retired mutable `plugin_config.permission`
    // source. E1 every entry reports `permission migration_required`; E2 no
    // config revision is inserted or advanced; E3 the audited transaction also
    // leaves its pre-recorded audit pending and commits no business state. C4 a
    // typed Toolset policy has no legacy section and
    // remains writable through the same ordinary authority. C5/C6 an archived
    // typed config reaches the atomic audit-only or audit+effect entry.
    // E5 neither path may advance config/effect/business-commit state; each
    // pre-recorded audit remains pending. C7 an
    // archived config has no current aggregate; C8 a published config mixes
    // archive with an authored edit across all four mutable entries. E6 only
    // the dedicated exact archive command may create Archived lifecycle.
    //
    // Decision table:
    // | rule | write entry | legacy section | effect |
    // | W1   | ordinary    | present        | error, absent draft |
    // | W2   | CAS         | present        | error, revision unchanged |
    // | W3   | audited     | present        | error, no draft; audit pending |
    // | W4   | ordinary    | absent/typed   | one durable revision |
    // | W5   | audited     | archived typed | error, no config; audit pending |
    // | W6   | audit+effect| archived typed | error, audit pending; no config/effect |
    // | W7   | ordinary    | absent+archived | error, absent draft |
    // | W8   | ordinary    | published->archived+edit | error, revision unchanged |
    // | W9   | CAS         | published->archived+edit | error, revision unchanged |
    // | W10  | audited     | published->archived+edit | error, audit pending |
    // | W11  | audit+effect| published->archived+edit | error, audit pending; no effect |
    let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
    let plane = ConfigPlane::new(
        Arc::new(test_service()),
        store,
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    let scope = scope("permission-write-rules");

    let ordinary = legacy_permission_config("ordinary-legacy");
    let error = plane.put(&scope, &ordinary).await.expect_err("W1");
    assert!(
        error.contains("permission migration_required"),
        "W1: {error}"
    );
    assert!(
        plane
            .get_versioned(&scope, &ordinary.id)
            .await
            .unwrap()
            .is_none(),
        "W1/E2"
    );

    let mut cas = agent_config("cas-legacy");
    plane.put(&scope, &cas).await.expect("CAS seed");
    let before = plane.get_versioned(&scope, &cas.id).await.unwrap().unwrap();
    cas.plugin_config = legacy_permission_config(&cas.id).plugin_config;
    let error = plane
        .put_if_revision(&scope, &cas, before.revision)
        .await
        .expect_err("W2");
    assert!(
        error.contains("permission migration_required"),
        "W2: {error}"
    );
    assert_eq!(
        plane
            .get_versioned(&scope, &cas.id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        before.revision,
        "W2/E2"
    );

    let audited = legacy_permission_config("audited-legacy");
    let audit = awaken_agent_config::ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "permission-audit-1".into(),
        summary: "must not commit retired permission authoring".into(),
    };
    assert_eq!(
        plane.record_management_audit(&scope, &audit).await.unwrap(),
        AuditedConfigWrite::Applied,
        "W3 begin"
    );
    let error = plane
        .put_with_audit(&scope, &audited, 0, &audit)
        .await
        .expect_err("W3");
    assert!(
        error.contains("permission migration_required"),
        "W3: {error}"
    );
    assert!(
        plane
            .get_versioned(&scope, &audited.id)
            .await
            .unwrap()
            .is_none(),
        "W3/E2"
    );
    assert!(
        plane
            .get_management_audit(&scope, &audit.tool, &audit.call_id)
            .await
            .unwrap()
            .is_some_and(|entry| !entry.business_committed),
        "W3/E3 pending audit intent"
    );

    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };
    let mut typed = agent_config("typed-controlled");
    typed.toolsets = vec![ToolsetPolicy {
        source: ToolsetSource::Agent,
        default: ToolExecutionPolicy::default(),
        overrides: ["write", "edit", "bash"]
            .into_iter()
            .map(|name| {
                ToolPolicyOverride::new(
                    name,
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAsk,
                    },
                )
            })
            .collect(),
    }];
    plane.put(&scope, &typed).await.expect("W4");
    assert_eq!(
        plane
            .get_versioned(&scope, &typed.id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        1,
        "W4"
    );

    let archived_id = "archived-audited";
    let archived_source = agent_config(archived_id);
    plane.put(&scope, &archived_source).await.expect("W5 seed");
    let before_archive = plane
        .get_versioned(&scope, archived_id)
        .await
        .unwrap()
        .unwrap();
    let mut archived = before_archive.config;
    archived.archived_at = Some("2026-08-30T00:00:00Z".into());
    assert!(matches!(
        plane
            .archive_if_revision(&scope, &archived, before_archive.revision)
            .await
            .unwrap(),
        ConfigWrite::Applied { revision: 2 }
    ));
    let mut edited_archived = archived;
    edited_archived.instructions.push_str(" forbidden");
    let archived_audit = awaken_agent_config::ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "archived-audit".into(),
        summary: "must not rewrite archived Agent".into(),
    };
    assert_eq!(
        plane
            .record_management_audit(&scope, &archived_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "W5 begin"
    );
    let error = plane
        .put_with_audit(&scope, &edited_archived, 2, &archived_audit)
        .await
        .expect_err("W5");
    assert!(error.contains("dedicated command"), "W5/E5: {error}");
    assert_eq!(
        plane
            .get_versioned(&scope, archived_id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        2,
        "W5/E5"
    );
    assert!(
        plane
            .get_management_audit(&scope, &archived_audit.tool, &archived_audit.call_id)
            .await
            .unwrap()
            .is_some_and(|entry| !entry.business_committed),
        "W5/E5 pending audit intent"
    );

    let effect_audit = awaken_agent_config::ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "archived-effect".into(),
        summary: "must not journal resources for archived Agent".into(),
    };
    let effect = awaken_agent_config::ManagementEffect::UpsertAgentInputs {
        config: AgentInputConfig {
            agent_id: archived_id.into(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        },
    };
    assert_eq!(
        plane
            .record_management_audit(&scope, &effect_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "W6 begin"
    );
    let error = plane
        .put_with_audit_effect(&scope, &edited_archived, 2, &effect_audit, Some(&effect))
        .await
        .expect_err("W6");
    assert!(error.contains("dedicated command"), "W6/E5: {error}");
    assert_eq!(
        plane
            .get_versioned(&scope, archived_id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        2,
        "W6/E5"
    );

    let mut absent_archived = agent_config("absent-archived");
    absent_archived.archived_at = Some("2026-08-30T00:00:00Z".into());
    let error = plane.put(&scope, &absent_archived).await.expect_err("W7");
    assert!(error.contains("dedicated command"), "W7/E6: {error}");
    assert!(
        plane
            .get_versioned(&scope, &absent_archived.id)
            .await
            .unwrap()
            .is_none(),
        "W7/E6"
    );

    let lifecycle_id = "mixed-lifecycle";
    plane
        .put(&scope, &agent_config(lifecycle_id))
        .await
        .expect("W8-W11 seed");
    let lifecycle_before = plane
        .get_versioned(&scope, lifecycle_id)
        .await
        .unwrap()
        .unwrap();
    let mut mixed_lifecycle = lifecycle_before.config;
    mixed_lifecycle.archived_at = Some("2026-08-30T00:00:00Z".into());
    mixed_lifecycle.instructions.push_str(" forbidden");
    for (rule, result) in [
        ("W8", plane.put(&scope, &mixed_lifecycle).await),
        (
            "W9",
            plane
                .put_if_revision(&scope, &mixed_lifecycle, lifecycle_before.revision)
                .await
                .map(|_| ()),
        ),
    ] {
        let error = result.expect_err(rule);
        assert!(error.contains("dedicated command"), "{rule}/E6: {error}");
    }
    let mixed_audit = awaken_agent_config::ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "mixed-lifecycle-audit".into(),
        summary: "must not combine archive and authoring".into(),
    };
    assert_eq!(
        plane
            .record_management_audit(&scope, &mixed_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "W10 begin"
    );
    let error = plane
        .put_with_audit(
            &scope,
            &mixed_lifecycle,
            lifecycle_before.revision,
            &mixed_audit,
        )
        .await
        .expect_err("W10");
    assert!(error.contains("dedicated command"), "W10/E6: {error}");
    assert!(
        plane
            .get_management_audit(&scope, &mixed_audit.tool, &mixed_audit.call_id)
            .await
            .unwrap()
            .is_some_and(|entry| !entry.business_committed),
        "W10/E6 pending audit intent"
    );
    let mixed_effect_audit = awaken_agent_config::ManagementAuditRecord {
        call_id: "mixed-lifecycle-effect".into(),
        ..mixed_audit
    };
    let mixed_effect = awaken_agent_config::ManagementEffect::UpsertAgentInputs {
        config: AgentInputConfig {
            agent_id: lifecycle_id.into(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        },
    };
    assert_eq!(
        plane
            .record_management_audit(&scope, &mixed_effect_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "W11 begin"
    );
    let error = plane
        .put_with_audit_effect(
            &scope,
            &mixed_lifecycle,
            lifecycle_before.revision,
            &mixed_effect_audit,
            Some(&mixed_effect),
        )
        .await
        .expect_err("W11");
    assert!(error.contains("dedicated command"), "W11/E6: {error}");
    assert!(
        plane
            .get_management_audit(
                &scope,
                &mixed_effect_audit.tool,
                &mixed_effect_audit.call_id,
            )
            .await
            .unwrap()
            .is_some_and(|entry| !entry.business_committed),
        "W11/E6 pending audit intent"
    );
    assert_eq!(
        plane
            .get_versioned(&scope, lifecycle_id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        lifecycle_before.revision,
        "W8-W11/E6"
    );
    assert!(
        plane
            .pending_management_effects(&scope)
            .await
            .unwrap()
            .is_empty(),
        "W11/E6"
    );
    assert!(
        plane
            .get_management_audit(&scope, &effect_audit.tool, &effect_audit.call_id)
            .await
            .unwrap()
            .is_some_and(|entry| !entry.business_committed),
        "W6/E5 pending audit intent"
    );
    assert!(
        plane
            .pending_management_effects(&scope)
            .await
            .unwrap()
            .is_empty(),
        "W6/E5"
    );
}

#[tokio::test]
async fn rejected_legacy_edit_preserves_draft_generation_and_published_fingerprint() {
    // Cause/effect graph: C1 a pre-canonical mutable draft and its immutable
    // publication already exist; C2 GET is read-only; C3 the next legacy write
    // is rejected. Effects: E1 GET cannot advance the draft generation; E2 the
    // failed write advances neither generation nor publication identity; E3 the
    // exact legacy publication remains readable for Runtime compatibility.
    // Decision rules: R1=C1+C2 -> stable generation/fingerprint;
    // R2=C1+C3 -> migration_required + the same durable identities.
    let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
    let plane = ConfigPlane::new(
        Arc::new(test_service()),
        store.clone(),
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    let scope = scope("legacy-read-stability");
    let legacy = legacy_permission_config("legacy-published");
    store
        .put_config_scoped(&scope, &legacy)
        .await
        .expect("seed historical draft without using a mutable application write");
    let before = plane
        .get_versioned(&scope, &legacy.id)
        .await
        .unwrap()
        .unwrap();
    let publication = plane.publish(&scope, &legacy.id).await.unwrap();

    let reread = plane
        .get_versioned(&scope, &legacy.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reread.revision, before.revision, "R1/E1");
    assert_eq!(reread.config, legacy, "R1/E1");

    let mut edited = reread.config;
    edited.instructions.push_str(" safely");
    let error = plane
        .put_if_revision(&scope, &edited, before.revision)
        .await
        .expect_err("R2");
    assert!(
        error.contains("permission migration_required"),
        "R2: {error}"
    );
    let after = plane
        .get_versioned(&scope, &legacy.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.revision, before.revision, "R2/E2");
    assert_eq!(after.config, legacy, "R2/E2");
    let frozen = plane
        .publication(&scope, &publication.fingerprint)
        .await
        .unwrap()
        .expect("R2/E3 legacy publication");
    assert_eq!(frozen.fingerprint, publication.fingerprint, "R1/R2/E2");
    assert!(
        frozen
            .snapshot
            .resolved_spec
            .plugin_config
            .get("permission")
            .is_some(),
        "R2/E3"
    );
}

#[tokio::test]
async fn lifecycle_archive_preserves_legacy_policy_bytes_and_rejects_mixed_edits() {
    // Cause/effect graph: C0 ordinary CAS attempts the archive transition;
    // C1 a historical legacy draft is archived with the
    // exact stored payload except for lifecycle fields; C2 the same request
    // also edits authored content; C3 ordinary CAS sees the same legacy bytes;
    // C4 an already archived revision is submitted again; C5 expected revision
    // is stale.
    // Effects: E0 C0 is rejected without a revision or withdrawal side effect;
    // E1 C1 advances one revision and preserves permission JSON byte
    // semantics; E2 C2 is rejected with no revision; E3 C3 remains
    // migration_required; E4 archived replay cannot manufacture a revision;
    // E5 stale expected returns Conflict with no write. The lifecycle port
    // therefore removes execution authority without reopening general legacy authoring.
    //
    // Decision table:
    // | rule | entry     | policy bytes | non-lifecycle delta | effect |
    // | L0   | ordinary  | unchanged    | archive transition  | error, zero write |
    // | L1   | archive   | unchanged    | no                  | archived revision |
    // | L2   | archive   | unchanged    | yes                 | error, zero write |
    // | L3   | ordinary  | legacy       | any                 | migration_required |
    // | L4   | archive   | already archived | no              | error, zero write |
    // | L5   | archive   | unchanged    | stale expected      | conflict, zero write |
    let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
    let plane = ConfigPlane::new(
        Arc::new(test_service()),
        store.clone(),
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    let scope = scope("legacy-lifecycle");

    let ordinary = agent_config("ordinary-archive");
    plane.put(&scope, &ordinary).await.unwrap();
    let ordinary_before = plane
        .get_versioned(&scope, &ordinary.id)
        .await
        .unwrap()
        .unwrap();
    let mut ordinary_archived = ordinary_before.config;
    ordinary_archived.archived_at = Some("2026-08-30T00:00:00Z".into());
    let error = plane
        .put_if_revision(&scope, &ordinary_archived, ordinary_before.revision)
        .await
        .expect_err("L0");
    assert!(error.contains("dedicated command"), "L0/E0: {error}");
    assert_eq!(
        plane
            .get_versioned(&scope, &ordinary.id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        ordinary_before.revision,
        "L0/E0"
    );

    for id in ["archive-only", "archive-plus-edit"] {
        store
            .put_config_scoped(&scope, &legacy_permission_config(id))
            .await
            .unwrap();
    }
    let before = plane
        .get_versioned(&scope, "archive-only")
        .await
        .unwrap()
        .unwrap();
    let legacy_policy = before.config.plugin_config["permission"].clone();
    let mut archived = before.config.clone();
    archived.archived_at = Some("2026-08-30T00:00:00Z".into());
    assert!(matches!(
        plane
            .archive_if_revision(&scope, &archived, before.revision)
            .await
            .expect("L1"),
        ConfigWrite::Applied { revision: 2 }
    ));
    let after = plane
        .get_versioned(&scope, "archive-only")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.config.plugin_config["permission"], legacy_policy,
        "L1/E1"
    );

    let mixed_before = plane
        .get_versioned(&scope, "archive-plus-edit")
        .await
        .unwrap()
        .unwrap();
    let mut mixed = mixed_before.config.clone();
    mixed.archived_at = Some("2026-08-30T00:00:00Z".into());
    mixed.instructions.push_str(" changed");
    let error = plane
        .archive_if_revision(&scope, &mixed, mixed_before.revision)
        .await
        .expect_err("L2");
    assert!(error.contains("lifecycle-only"), "L2/E2: {error}");
    assert_eq!(
        plane
            .get_versioned(&scope, "archive-plus-edit")
            .await
            .unwrap()
            .unwrap()
            .revision,
        mixed_before.revision,
        "L2/E2"
    );

    let error = plane
        .put_if_revision(&scope, &mixed_before.config, mixed_before.revision)
        .await
        .expect_err("L3");
    assert!(error.contains("permission migration_required"), "L3/E3");

    let replay_error = plane
        .archive_if_revision(&scope, &after.config, after.revision)
        .await
        .expect_err("L4");
    assert!(
        replay_error.contains("source must be published or disabled"),
        "L4/E4"
    );
    assert_eq!(
        plane
            .get_versioned(&scope, "archive-only")
            .await
            .unwrap()
            .unwrap()
            .revision,
        after.revision,
        "L4/E4"
    );

    let mut stale_candidate = mixed_before.config.clone();
    stale_candidate.archived_at = Some("2026-08-30T00:00:00Z".into());
    assert!(matches!(
        plane
            .archive_if_revision(&scope, &stale_candidate, mixed_before.revision + 1)
            .await
            .expect("L5"),
        ConfigWrite::Conflict {
            current_revision: Some(revision)
        } if revision == mixed_before.revision
    ));
    assert_eq!(
        plane
            .get_versioned(&scope, "archive-plus-edit")
            .await
            .unwrap()
            .unwrap()
            .revision,
        mixed_before.revision,
        "L5/E5"
    );
}
