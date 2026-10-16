// @formal evidence session_resources.explicit_empty_manifest_revokes_runtime_projection
//! Frozen resource manifests, mounts, and activation validation.

use super::*;

/// G1 — prompt and mount are derived from the same effective input, including access.
#[tokio::test]
async fn told_equals_mounted_the_prompt_path_and_access_match_the_realized_mount() {
    use awaken_provisioning_contract::MountAccess;
    let host = (crate::host::with_test_resource_validation(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ))
    .build()
    .expect("validated Host dependencies");
    let store_id = test_memory_store_id();
    host.resources
        .memory_stores
        .writer()
        .create(&store_id, "/seed.md", "seed")
        .await
        .expect("seed memory");

    let resource = TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    };
    let managed = managed_with_resource_source(host.clone());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![resource]);
    managed
        .install_test_session_init("t-g1", init)
        .await
        .unwrap();

    let realized = "/mnt/memory";
    let prompts = host.thread_session_prompts("t-g1");
    assert!(
        prompts.iter().any(|p| p.contains(realized)),
        "the compiled prompt names the realized path {realized}: {prompts:?}"
    );
    assert!(
        prompts.iter().any(|p| p.contains("read-only")),
        "a read-only binding is described read-only: {prompts:?}"
    );
    let mount = &host.realizer.sandbox_spec("t-g1").mounts[0];
    assert_eq!(mount.mount_path, realized);
    assert_eq!(mount.access, MountAccess::ReadOnly);
}

#[tokio::test]
async fn managed_multi_memory_mounts_are_backend_neutral_pairwise() {
    // Cause graph:
    // C1 backend={default/native, acp:claude, acp:codex};
    // C2 store count={1,2,8}; C3 access={all RO, all RW, alternating};
    // C4 instructions={absent,present}. Effects: E1 every store becomes one
    // mount/prompt/binding; E2 access and instructions survive projection;
    // E3 no automatic-memory binding is selected by ordinary Managed resources;
    // E4 every backend receives the same Managed absolute /mnt/memory path, never
    // a host-relative `.mnt` carrier.
    // Constraint: backend selection owns execution only and cannot change
    // resource realization. The nine rows below are a strength-2 covering array:
    // every value pair across C1..C4 occurs at least once. This test verifies
    // that property before executing the rows, rather than trusting the table.
    // R7 extends P5 with two distinct bindings to the same store, one RO and
    // one RW. The per-input compiled mount determines each MemoryBinding before
    // merge; the earlier mount cannot determine the later binding's access.
    #[derive(Clone, Copy)]
    struct PairwiseCase {
        rule: &'static str,
        backend: &'static str,
        count: &'static str,
        access: &'static str,
        instructions: &'static str,
    }

    impl PairwiseCase {
        fn values(self) -> [&'static str; 4] {
            [self.backend, self.count, self.access, self.instructions]
        }
    }

    let cases = [
        PairwiseCase {
            rule: "P1",
            backend: "default",
            count: "1",
            access: "ro",
            instructions: "absent",
        },
        PairwiseCase {
            rule: "P2",
            backend: "acp:claude",
            count: "1",
            access: "rw",
            instructions: "absent",
        },
        PairwiseCase {
            rule: "P3",
            backend: "acp:codex",
            count: "1",
            access: "mixed",
            instructions: "present",
        },
        PairwiseCase {
            rule: "P4",
            backend: "default",
            count: "2",
            access: "rw",
            instructions: "present",
        },
        PairwiseCase {
            rule: "P5",
            backend: "acp:claude",
            count: "2",
            access: "mixed",
            instructions: "absent",
        },
        PairwiseCase {
            rule: "P6",
            backend: "acp:codex",
            count: "2",
            access: "ro",
            instructions: "absent",
        },
        PairwiseCase {
            rule: "P7",
            backend: "default",
            count: "8",
            access: "mixed",
            instructions: "absent",
        },
        PairwiseCase {
            rule: "P8",
            backend: "acp:claude",
            count: "8",
            access: "ro",
            instructions: "present",
        },
        PairwiseCase {
            rule: "P9",
            backend: "acp:codex",
            count: "8",
            access: "rw",
            instructions: "absent",
        },
    ];

    for left in 0..4 {
        for right in (left + 1)..4 {
            let left_levels = cases
                .iter()
                .map(|case| case.values()[left])
                .collect::<std::collections::BTreeSet<_>>();
            let right_levels = cases
                .iter()
                .map(|case| case.values()[right])
                .collect::<std::collections::BTreeSet<_>>();
            let observed = cases
                .iter()
                .map(|case| (case.values()[left], case.values()[right]))
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                observed.len(),
                left_levels.len() * right_levels.len(),
                "pairwise axes {left}/{right} are incomplete"
            );
        }
    }

    let host = (crate::host::with_test_resource_validation(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    for case in cases {
        let count = case.count.parse::<usize>().unwrap();
        let inputs = (0..count)
            .map(|index| TestInput {
                kind: "memory_store".into(),
                id: if case.rule == "P5" {
                    "p5-shared-store".into()
                } else {
                    format!("{}-store-{index}", case.rule)
                },
                mount_path: format!("/mnt/memory/{}-{index}", case.rule.to_lowercase()),
                access: match case.access {
                    "rw" => ResourceAccess::ReadWrite,
                    "mixed" if index % 2 == 1 => ResourceAccess::ReadWrite,
                    "ro" | "mixed" => ResourceAccess::ReadOnly,
                    other => panic!("unknown access pattern {other}"),
                },
                instructions: (case.instructions == "present")
                    .then(|| format!("{} memory {index}", case.rule)),
                initial_branch: None,
                initial_commit: None,
            })
            .collect::<Vec<_>>();
        let expected_access = inputs.iter().map(|input| input.access).collect::<Vec<_>>();
        let thread = format!("pairwise-{}", case.rule.to_lowercase());
        let mut init = bare_session("agent", host.local_workspace());
        init.runtime = Some(case.backend.into());
        let resources = effective_resources(inputs);
        init.resources = resources.clone();
        managed
            .install_complete_test_session(&thread, init)
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", case.rule));
        managed
            .apply_session_inputs(
                &thread,
                &resource_transition(host.local_workspace(), 0, Default::default(), 0, resources),
            )
            .await
            .unwrap_or_else(|error| panic!("{} compiles the exact transition: {error}", case.rule));

        let spec = host.realizer.sandbox_spec(&thread);
        assert_eq!(spec.mounts.len(), count, "{} E1", case.rule);
        for (index, mount) in spec.mounts.iter().enumerate() {
            let managed_path = format!("/mnt/memory/{}-{index}", case.rule.to_lowercase());
            assert_eq!(mount.mount_path, managed_path, "{} E4/{index}", case.rule);
            let expected = match expected_access[index] {
                ResourceAccess::ReadOnly => awaken_provisioning_contract::MountAccess::ReadOnly,
                ResourceAccess::ReadWrite => awaken_provisioning_contract::MountAccess::ReadWrite,
            };
            assert_eq!(mount.access, expected, "{} E2/{index}", case.rule);
        }
        let prompts = host.thread_session_prompts(&thread);
        assert_eq!(prompts.len(), count, "{} E1 prompts", case.rule);
        for (index, prompt) in prompts.iter().enumerate() {
            let managed_path = format!("/mnt/memory/{}-{index}", case.rule.to_lowercase());
            assert!(prompt.contains(&managed_path), "{} E4/{index}", case.rule);
            assert!(!prompt.contains(".mnt/"), "{} E4/{index}", case.rule);
        }
        assert_eq!(
            prompts.iter().all(|prompt| prompt.contains(case.rule)),
            case.instructions == "present",
            "{} E2 instructions",
            case.rule
        );
        let (binding_count, automatic_absent) = host
            .installed_sessions
            .realizations()
            .read_realization(&thread, |r| {
                (r.staging.memory_bindings.len(), r.staging.memory.is_none())
            })
            .unwrap();
        assert_eq!(binding_count, count, "{} E1 bindings", case.rule);
        assert!(automatic_absent, "{} E3", case.rule);
        host.installed_sessions
            .realizations()
            .read_realization(&thread, |r| {
                for (index, access) in expected_access.iter().enumerate() {
                    let binding = &r.staging.memory_bindings[&format!("test-input-{index}")];
                    assert_eq!(
                        binding.extraction_enabled,
                        *access == ResourceAccess::ReadWrite,
                        "{} R7 binding {index} keeps its own access",
                        case.rule
                    );
                }
            })
            .unwrap();
    }
}

#[tokio::test]
async fn automatic_memory_requires_one_explicit_existing_binding() {
    // The state selection now belongs to the original ResourceStaging view.
    // It mutates only realization under the same slot lock: no binding clears
    // selection, an absent named binding clears it before the same error, and
    // an exact named binding returns that same Arc. Host execution alone wraps
    // it in MemoryRuntime.bound; neither selection nor wrapping starts a Run.
    // A1-A4 below keep the original configuration/error/identity effects.
    // Automatic-memory cause/effect graph:
    // C1 published Agent selects the Awaken memory plugin; C2 config supplies
    // binding_id; C3 that id exists in the Session's standard mount bindings.
    // E1 leave automatic memory inactive; E2 reject ambiguous/missing config;
    // E3 select exactly the authored binding and never the first array entry.
    // Decision table: A1 !C1=>E1; A2 C1&&!C2=>E2;
    // A3 C1+C2&&!C3=>E2/E1; A4 C1+C2+C3=>E3.
    // F1 each row selects one real SQLite/Persistence before Host construction,
    // then creates/assigns its Root before Runtime installation. This supplies
    // Environment authority, not a memory binding: A2/A3 still fail, A4 must
    // select the second explicitly authored existing store, and all four
    // context-only requests create zero Runs. No mock authorization or
    // first-array-entry fallback may turn absent config into a valid binding.

    let cases = [
        ("A1", false, Some("test-input-0"), None, None),
        (
            "A2",
            true,
            None,
            None,
            Some("requires an explicit `memory.binding_id`"),
        ),
        (
            "A3",
            true,
            Some("missing-binding"),
            None,
            Some("is not mounted for this Session"),
        ),
        ("A4", true, Some("test-input-1"), Some(1_usize), None),
    ];

    for (rule, plugin_selected, binding_id, expected_index, expected_error) in cases {
        let plugin_ids = if plugin_selected {
            vec![awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()]
        } else {
            Vec::new()
        };
        let plugin_config = if plugin_selected {
            std::collections::BTreeMap::from([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                binding_id.map_or_else(
                    || serde_json::json!({}),
                    |binding_id| serde_json::json!({"binding_id": binding_id}),
                ),
            )])
        } else {
            std::collections::BTreeMap::new()
        };
        let snapshot = crate::config::server_config(
            "agent",
            "stub",
            &HashSet::new(),
            &HashSet::new(),
            &plugin_ids,
            &plugin_config,
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        )
        .expect("literal model reference");
        let publications =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
                .expect("valid publication");
        let roots = ManagedSessionRootFixture::new().await;
        let dependencies = crate::host::with_test_resource_validation(with_test_memory_mounter(
            crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
                .await
                .with_agent_publications(Arc::new(publications)),
        ));
        let host = roots
            .configure(dependencies)
            .build()
            .expect("validated Host dependencies");
        let stores = [test_memory_store_id(), test_memory_store_id()];
        for store in &stores {
            host.resources
                .memory_stores
                .writer()
                .create(store, "/seed.md", rule)
                .await
                .unwrap();
        }
        let mut init = bare_session("agent", host.local_workspace());
        init.resources = effective_resources(
            stores
                .iter()
                .enumerate()
                .map(|(index, store)| TestInput {
                    kind: "memory_store".into(),
                    id: store.clone(),
                    mount_path: format!("/mnt/memory/{rule}-{index}"),
                    access: ResourceAccess::ReadWrite,
                    instructions: None,
                    initial_branch: None,
                    initial_commit: None,
                })
                .collect(),
        );
        let managed = managed_with_resource_source(host.clone());
        let thread = format!("automatic-{rule}");
        roots
            .install_complete_test_session(&managed, &thread, init)
            .await
            .unwrap();
        assert!(
            host.memory_for_thread(&thread).is_none(),
            "{rule} precondition"
        );

        let result = host.ctx_for(&thread, Some("agent")).await;
        match expected_error {
            Some(fragment) => {
                let error = match result {
                    Ok(_) => panic!("{rule}: invalid automatic binding must fail"),
                    Err(error) => error,
                };
                assert!(error.to_string().contains(fragment), "{rule}: {error}");
                assert!(host.memory_for_thread(&thread).is_none(), "{rule} E1");
            }
            None => {
                result.unwrap_or_else(|error| panic!("{rule}: {error}"));
                let selected = host.memory_for_thread(&thread);
                match expected_index {
                    Some(index) => assert_eq!(
                        selected
                            .as_ref()
                            .map(|memory| memory.binding().memory_store_id.as_str()),
                        Some(stores[index].as_str()),
                        "{rule} E3"
                    ),
                    None => assert!(selected.is_none(), "{rule} E1"),
                }
            }
        }
        assert!(
            host.commit_for_read(&thread)
                .await
                .unwrap()
                .authoritative_latest_run(&ThreadId(thread.clone()))
                .await
                .unwrap()
                .is_none(),
            "{rule}: context construction starts zero Runs"
        );
    }
}

/// Runtime stages exactly the effective resource list it receives and adds no hidden
/// Agent defaults of its own. Replacement is a Session-control-plane decision.
#[tokio::test]
async fn runtime_stages_exactly_the_effective_resource_list() {
    let host = (crate::host::with_test_resource_validation(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ))
    .build()
    .expect("validated Host dependencies");
    let s2 = test_memory_store_id();
    host.resources
        .memory_stores
        .writer()
        .create(&s2, "/wire.md", "WIRE-BYTES")
        .await
        .unwrap();

    let managed = managed_with_resource_source(host.clone());

    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: s2.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    managed
        .install_test_session_init("t-g3", init)
        .await
        .unwrap();

    // Exactly one memory mount at that path, and it is the wire store S2.
    let spec = host.realizer.sandbox_spec("t-g3");
    let at_path: Vec<_> = spec
        .mounts
        .iter()
        .filter(|mount| mount.mount_path == "/mnt/memory")
        .collect();
    assert_eq!(
        at_path.len(),
        1,
        "one mount wins the path, not both: {:?}",
        spec.mounts
    );
    assert_eq!(
        memory_mount_store_id(at_path[0]),
        s2,
        "the carried effective store is the only staged store"
    );

    assert_eq!(
        memory_mount_store_id(&host.realizer.sandbox_spec("t-g3").mounts[0]),
        s2
    );
}

/// G4 — an effective Memory input whose backing store is absent fails closed.
#[tokio::test]
async fn a_bound_resource_with_a_missing_backing_store_fails_the_session_closed() {
    let host = ((crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .with_session_execution(
            crate::SessionExecutionDependencies::default()
                .with_resource_validator(resource_registry().await),
        ))
    .build()
    .expect("validated Host dependencies");
    let managed = crate::ManagedHost::new(host.clone());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: "never-seeded-store".into(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    let result = managed.install_test_session_init("t-g4", init).await;
    assert!(
        result.is_err(),
        "a binding to a missing backing store must fail closed, not mount empty"
    );
}

#[tokio::test]
async fn activation_validates_the_frozen_config_without_selecting_current_again() {
    use awaken_resource_contract::{
        ChangeMemoryStoreState, ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition,
        PublishMemoryStoreConfig, RegisterMemoryStore, ResourceAdministration as _, ResourceState,
    };
    use awaken_session_configuration_contract::resource::ResolvedInputSource;

    let catalog = resource_registry().await;
    let host = ((crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .with_session_execution(
            crate::SessionExecutionDependencies::default().with_resource_validator(catalog.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let workspace = host.local_workspace().to_string();
    let store_id = test_memory_store_id();

    catalog
        .register_memory_store(RegisterMemoryStore {
            definition: MemoryStoreDefinition {
                id: store_id.clone().into(),
                workspace_id: workspace.clone(),
                name: "memory".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            initial_config: MemoryStoreConfigVersion {
                memory_store_id: store_id.clone().into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        })
        .await
        .expect("register frozen-config MemoryStore");
    catalog
        .publish_memory_store_config(PublishMemoryStoreConfig {
            workspace_id: workspace.clone(),
            expected_current: ConfigVersion::INITIAL,
            config: MemoryStoreConfigVersion {
                memory_store_id: store_id.clone().into(),
                version: ConfigVersion(2),
                retention_policy: Default::default(),
            },
        })
        .await
        .expect("publish MemoryStore config V2");

    let manifest = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    let managed = crate::ManagedHost::new(host.clone());
    let mut valid = bare_session("a", &workspace);
    valid.resources = manifest.clone();
    managed
        .install_test_session_init("frozen-v1", valid)
        .await
        .expect("v1 remains valid after current advances to v2");

    let binding_id = manifest.inputs()[0].binding_id.clone();
    let missing = manifest
        .update_input(&binding_id, |input| {
            let ResolvedInputSource::MemoryStore { config, .. } = &mut input.source else {
                panic!("expected MemoryStore input");
            };
            config.version = ConfigVersion(3);
        })
        .unwrap();
    let mut invalid = bare_session("a", &workspace);
    invalid.resources = missing;
    let error = managed
        .install_test_session_init("missing-v3", invalid)
        .await
        .expect_err("an absent frozen config must fail closed");
    assert!(error.message.contains("config version"));
    assert!(host.realizer.sandbox_spec("missing-v3").mounts.is_empty());

    catalog
        .change_memory_store_state(ChangeMemoryStoreState {
            workspace_id: workspace.clone(),
            id: store_id.clone().into(),
            state: ResourceState::Archived,
        })
        .await
        .expect("archive frozen-config MemoryStore");
    let error = match run_prepared_session(
        &managed,
        "a",
        "frozen-v1",
        vec![ContentBlock::Text {
            text: "must not reach the model".into(),
        }],
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("live lifecycle state must deny a later operation"),
    };
    assert!(error.message.contains("not active"));
}

#[tokio::test]
async fn replacing_a_manifest_removes_the_old_delivered_skill_tree_immediately() {
    use awaken_skill_store::{SkillBundleFile, SkillDefinition, SkillVersion, bundle_sha256};

    let storage = tempfile::tempdir().expect("storage");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("a")
        .model(test_model_binding())
        .tools(crate::config::advertised_tools(
            &HashSet::new(),
            &HashSet::new(),
            &[],
        ))
        .agent_bindings(awaken_runtime_contract::agent_bindings::AgentBindings {
            skills: vec![awaken_agent_contract::AgentSkillBinding::custom("governed")],
            toolsets: vec![awaken_agent_contract::ToolsetPolicy {
                source: awaken_agent_contract::ToolsetSource::Agent,
                default: awaken_agent_contract::ToolExecutionPolicy::default(),
                overrides: Vec::new(),
            }],
            ..Default::default()
        })
        .build();
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("one immutable governed Agent");
    let host = (crate::host::with_test_resource_validation(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path())
            .await
            .with_agent_publications(Arc::new(publications)),
    ))
    .build()
    .expect("validated Host dependencies");
    let workspace = host.local_workspace().to_string();
    let files = vec![
        SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\nname: governed\ndescription: governed\n---\nuse it".to_vec(),
            executable: false,
        },
        SkillBundleFile {
            path: "scripts/old.sh".into(),
            content: b"exit 0".to_vec(),
            executable: false,
        },
    ];
    let hash = bundle_sha256(&files);
    host.tools
        .skills
        .create(
            SkillDefinition {
                id: "governed".into(),
                workspace_id: workspace.clone(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: Default::default(),
            },
            SkillVersion {
                id: "skver_governed_1".into(),
                skill_id: "governed".into(),
                version: 1,
                name: "governed".into(),
                description: "governed".into(),
                directory: "/skills/governed".into(),
                bundle_sha256: hash.clone(),
                files,
                created_unix_nanos: 0,
            },
        )
        .await
        .expect("durable SkillStore")
        .expect("create Skill");
    let managed = managed_with_resource_source(host.clone());
    let mut init = bare_session("a", &workspace);
    let resources = init
        .resources
        .with_skills(vec![crate::session_resource::ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: "governed".into(),
            version: 1,
            bundle_sha256: hash,
        }])
        .unwrap();
    init.resources = resources.clone();
    install_complete_test_session_for_realization(&managed, "skill-revoke", init)
        .await
        .expect("prepare pinned Skill");
    managed
        .apply_session_inputs(
            "skill-revoke",
            &resource_transition(&workspace, 0, Default::default(), 0, resources),
        )
        .await
        .expect("compile pinned Skill bytes under the exact transition");
    run_prepared_session(
        &managed,
        "a",
        "skill-revoke",
        vec![ContentBlock::Text {
            text: "open the environment".into(),
        }],
    )
    .await
    .expect("materialize Skill");
    let environment = host
        .realizer
        .session_environment("skill-revoke")
        .await
        .expect("resident Skill environment");
    assert!(
        environment
            .skills()
            .scan_skill_dir(crate::skill_delivery::DELIVERED_SKILLS_SUBDIR)
            .expect("scan delivered Skill projection")
            .iter()
            .any(|skill| skill.id == "governed"),
        "pinned Skill tree is readable through the Environment"
    );
    let before_manifest = host
        .realizer
        .staging
        .thread_resource_manifest("skill-revoke")
        .expect("installed manifest");

    managed
        .apply_session_inputs(
            "skill-revoke",
            &resource_transition(
                &before_manifest.workspace_id,
                before_manifest.revision,
                before_manifest.resources.clone(),
                before_manifest.revision + 1,
                crate::session_resource::ResolvedSessionResources::default(),
            ),
        )
        .await
        .expect("replace with explicit empty Skill selection");
    assert!(
        environment
            .skills()
            .scan_skill_dir(crate::skill_delivery::DELIVERED_SKILLS_SUBDIR)
            .expect("scan after Skill removal")
            .is_empty(),
        "the old delivered tree is gone before another Run can read it"
    );
}

/// G5 — the effective resource reference crosses the node boundary, so a worker needs
/// access to the resource data plane but not to the Agent authoring repository.
#[tokio::test]
async fn an_effective_resource_mounts_on_a_worker_without_the_binding_repository() {
    let db_less = (crate::host::with_test_resource_validation(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ))
    .build()
    .expect("validated Host dependencies");
    let store_id = test_memory_store_id();
    db_less
        .resources
        .memory_stores
        .writer()
        .create(&store_id, "/carried.md", "CARRIED-BYTES")
        .await
        .expect("seed");
    let managed_worker = managed_with_resource_source(db_less.clone());
    let mut init = bare_session("a", db_less.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    managed_worker
        .install_test_session_init("t-g5-worker", init)
        .await
        .unwrap();
    assert_eq!(
        memory_mount_store_id(&db_less.realizer.sandbox_spec("t-g5-worker").mounts[0]),
        store_id,
        "the effective input is sufficient for a DB-less worker"
    );
    assert_eq!(
        db_less.realizer.sandbox_spec("t-g5-worker").mounts[0].access,
        awaken_provisioning_contract::MountAccess::ReadOnly,
        "the carried access is sufficient; no Host-side write-back registry exists"
    );
}
