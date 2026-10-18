use awaken_agent_contract::{AgentSkillBinding, AgentSkillKind};
use awaken_resource_contract::{SkillDefinition, SkillVersion};
use awaken_session_configuration_contract::resource::ResolvedSkillBinding;
use awaken_skill_store::{InMemorySkillStore, SkillBundleFile, bundle_sha256};
use std::sync::Arc;

fn version(id: &str, ordinal: u64, body: &str) -> SkillVersion {
    let files = vec![SkillBundleFile {
        path: "SKILL.md".into(),
        content: body.as_bytes().to_vec(),
        executable: false,
    }];
    SkillVersion {
        id: format!("skver-{id}-{ordinal}").into(),
        skill_id: id.into(),
        version: ordinal,
        name: id.into(),
        description: String::new(),
        directory: format!("/skills/{id}"),
        bundle_sha256: bundle_sha256(&files),
        files,
        created_unix_nanos: ordinal,
    }
}

#[tokio::test]
async fn empty_pinned_skill_set_needs_no_repository_but_real_references_fail_closed() {
    // K1 empty pins need no read authority; K5/K7 a real custom pin
    // without its source rejects. Catalog selection grants no byte read.
    let host = crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .unwrap();
    let catalog = &host.capabilities.tools.skills;
    let staging = host.realization_executor.staging();
    assert!(
        staging
            .load_pinned("ws-a", &[], None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        catalog
            .resolve("ws-a", &[AgentSkillBinding::custom("missing")])
            .await
            .is_err()
    );
    assert!(
        staging
            .load_pinned(
                "ws-a",
                &[ResolvedSkillBinding {
                    kind: AgentSkillKind::Custom,
                    skill_id: "missing".into(),
                    version: 1,
                    bundle_sha256: "sha256".into(),
                }],
                None,
                None,
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn skill_catalog_and_bundle_read_roles_are_selected_independently_before_construction() {
    // Cause/effect graph: C1 catalog application selected; C2 exact bundle
    // source selected; C3 custom v1 exists under ws-a. Catalog resolution and
    // pinned byte reads are independent roles, never fallback authorities.
    // K9a !C1+!C2 => both refuse; K9b C1+!C2 => selection only;
    // K9c !C1+C2 => exact bytes only; K9d C1+C2 => both succeed.
    // Every row uses the one production Host constructor and the actual Store
    // applications. Reading bytes must not install a Session or publish a
    // staged/active Resource generation. Missing C2 is K7, not empty success.
    let store = Arc::new(InMemorySkillStore::new());
    let selected = version("greet", 1, "---\ndescription: v1\n---\nONE");
    awaken_resource_contract::SkillStore::create(
        store.as_ref(),
        SkillDefinition {
            id: "greet".into(),
            workspace_id: "ws-a".into(),
            display_title: None,
            latest_version: 1,
            last_version: 1,
            timestamps: Default::default(),
        },
        selected.clone(),
    )
    .await
    .unwrap();
    let binding = ResolvedSkillBinding {
        kind: AgentSkillKind::Custom,
        skill_id: selected.skill_id.to_string(),
        version: selected.version,
        bundle_sha256: selected.bundle_sha256.clone(),
    };
    for (catalog, bytes) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut dependencies = crate::HostDependencies::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await;
        if catalog {
            let clock = dependencies.clock();
            dependencies = dependencies.with_skill_catalog_application(Arc::new(
                awaken_resource_application::StoreSkillCatalogApplication::new(
                    store.clone(),
                    clock,
                ),
            ));
        }
        if bytes {
            dependencies = dependencies.with_skill_bundle_source(Arc::new(
                awaken_resource_application::StoreSkillBundleSource::new(store.clone()),
            ));
        }
        let host = dependencies.build().unwrap();
        assert_eq!(
            host.capabilities
                .tools
                .skills
                .resolve("ws-a", &[AgentSkillBinding::custom("greet")])
                .await
                .is_ok(),
            catalog,
        );
        let loaded = host
            .realization_executor
            .staging()
            .load_pinned("ws-a", std::slice::from_ref(&binding), None, None)
            .await;
        assert_eq!(loaded.is_ok(), bytes);
        if let Ok(loaded) = loaded {
            assert_eq!(loaded, vec![selected.clone()]);
        }
        assert!(
            host.run_assembly
                .installed_sessions
                .session_ids()
                .expect("installed Sessions")
                .is_empty()
        );
    }
}

#[tokio::test]
async fn frozen_binding_keeps_v1_after_v2_and_is_workspace_scoped() {
    // K3/K4/K5/K9: actual Host construction selects the same Resources
    // applications; publishing v2 cannot replace frozen v1 or its Workspace.
    let host = crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .with_skill_store_backend(Arc::new(InMemorySkillStore::new()))
    .build()
    .unwrap();
    let catalog = &host.capabilities.tools.skills;
    let staging = host.realization_executor.staging();
    catalog
        .create(
            SkillDefinition {
                id: "greet".into(),
                workspace_id: "ws-a".into(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: Default::default(),
            },
            version("greet", 1, "---\ndescription: v1\n---\nONE"),
        )
        .await
        .unwrap()
        .unwrap();

    let frozen = catalog
        .resolve("ws-a", &[AgentSkillBinding::custom("greet")])
        .await
        .unwrap();
    catalog
        .append_version(
            "ws-a",
            "greet",
            version("greet", 2, "---\ndescription: v2\n---\nTWO"),
        )
        .await
        .unwrap()
        .unwrap();

    let loaded = staging
        .load_pinned("ws-a", &frozen, None, None)
        .await
        .unwrap();
    assert_eq!(loaded[0].version, 1);
    assert!(loaded[0].skill_md().unwrap().ends_with(b"ONE"));
    assert!(
        staging
            .load_pinned("ws-b", &frozen, None, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn prebuilt_and_custom_selectors_follow_one_resolution_table() {
    // Causes: prebuilt/custom source, latest/exact selector, repository
    // presence, and Workspace. Constraints: prebuilt v1 is runtime-owned;
    // custom bytes are Workspace-owned. Effects: both freeze exact hashes,
    // prebuilt needs no store, and custom exact selection never drifts.
    // Decision rules: S8 prebuilt latest/1 -> bundled v1; S9 prebuilt v2 ->
    // reject; S10 custom latest/exact -> selected store version.
    // K2/K9: bundled v1 is shared with catalog selection, while only the
    // staging component owns exact custom byte reads after construction.
    let host = crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .unwrap();
    let catalog = &host.capabilities.tools.skills;
    let staging = host.realization_executor.staging();
    for selector in ["latest", "1"] {
        let selection = AgentSkillBinding {
            kind: AgentSkillKind::Anthropic,
            skill_id: "xlsx".into(),
            version: selector.into(),
        };
        let frozen = catalog.resolve("ws-a", &[selection]).await.unwrap();
        assert_eq!(frozen[0].kind, AgentSkillKind::Anthropic, "S8");
        assert_eq!(
            staging
                .load_pinned("ws-a", &frozen, None, None)
                .await
                .unwrap()[0]
                .version,
            1
        );
    }
    assert!(
        catalog
            .resolve(
                "ws-a",
                &[AgentSkillBinding {
                    kind: AgentSkillKind::Anthropic,
                    skill_id: "xlsx".into(),
                    version: "2".into(),
                }],
            )
            .await
            .is_err(),
        "S9"
    );

    let host = crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .with_skill_store_backend(Arc::new(InMemorySkillStore::new()))
    .build()
    .unwrap();
    let catalog = &host.capabilities.tools.skills;
    catalog
        .create(
            SkillDefinition {
                id: "greet".into(),
                workspace_id: "ws-a".into(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: Default::default(),
            },
            version("greet", 1, "---\ndescription: v1\n---\nONE"),
        )
        .await
        .unwrap()
        .unwrap();
    catalog
        .append_version(
            "ws-a",
            "greet",
            version("greet", 2, "---\ndescription: v2\n---\nTWO"),
        )
        .await
        .unwrap()
        .unwrap();
    for (selector, expected) in [("latest", 2), ("1", 1)] {
        let frozen = catalog
            .resolve(
                "ws-a",
                &[AgentSkillBinding {
                    kind: AgentSkillKind::Custom,
                    skill_id: "greet".into(),
                    version: selector.into(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(frozen[0].version, expected, "S10");
    }
}
