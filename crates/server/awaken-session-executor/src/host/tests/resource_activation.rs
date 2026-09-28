// @formal evidence session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
//! Memory/File/Repository activation and terminal repository publication.

use super::*;

/// Runtime stages the effective resources supplied by the Session control plane. It
/// does not need the Agent binding repository, which keeps remote workers stateless.
#[tokio::test]
async fn install_test_session_init_mounts_an_effective_memory_resource() {
    use awaken_session_configuration_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));

    // Seed a memory store with known bytes. The control plane has already resolved
    // this resource into the SessionInit passed across the runtime boundary.
    let store_id = test_memory_store_id();
    host.resources
        .memory_stores
        .fs()
        .create(&store_id, "/facts.md", "the secret code is BANANA-42")
        .await
        .expect("seed memory");
    let managed = managed_with_resource_source(host.clone());

    let bare = |agent: &str| SessionInit {
        workspace_id: host.local_workspace().into(),
        agent_id: agent.into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: effective_resources(
            (agent == "a")
                .then(|| TestInput {
                    kind: "memory_store".into(),
                    id: store_id.clone(),
                    mount_path: "/mnt/memory".into(),
                    access: ResourceAccess::ReadWrite,
                    instructions: None,
                    initial_branch: None,
                    initial_commit: None,
                })
                .into_iter()
                .collect(),
        ),
        model: None,
        runtime: None,
        environment: session_environment(
            awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    };

    // An effective Session input mounts without an authoring repository on the host.
    managed
        .install_test_session_init("t-bound", bare("a"))
        .await
        .unwrap();
    let dump = serde_json::to_string(&host.sandbox_spec("t-bound").mounts).unwrap();
    assert!(
        dump.contains("mnt/memory"),
        "the bound memory store is mounted at its path: {dump}"
    );
    assert_eq!(
        memory_mount_store_id(&host.sandbox_spec("t-bound").mounts[0]),
        store_id
    );

    let mut read_only = bare("a");
    let binding_id = read_only.resources.inputs()[0].binding_id.clone();
    read_only.resources = read_only
        .resources
        .update_input(&binding_id, |input| {
            input.access = awaken_resource_contract::ResourceAccess::ReadOnly;
        })
        .unwrap();
    managed
        .install_test_session_init("t-read-only", read_only)
        .await
        .unwrap();
    assert_eq!(
        host.sandbox_spec("t-read-only").mounts[0].access,
        awaken_provisioning_contract::MountAccess::ReadOnly
    );

    // An empty effective input set mounts nothing.
    managed
        .install_test_session_init("t-unbound", bare("no-bindings"))
        .await
        .unwrap();
    let empty = serde_json::to_string(&host.sandbox_spec("t-unbound").mounts).unwrap();
    assert!(
        !empty.contains("BANANA-42"),
        "an unbound agent mounts nothing extra: {empty}"
    );
}

#[tokio::test]
async fn filesystem_free_session_projects_multiple_memories_as_semantic_tools() {
    // Cause/effect graph: C1 OnToolUse Environment; C2 the exact Agent toolset
    // disables every filesystem member but allows web ids without publishing a
    // web plugin; C3 two frozen
    // MemoryStore bindings with different access; C4 write has absent/current/
    // stale complete generation. Effects: E1 no Sandbox or Memory mount; E2 prompt names
    // every binding and semantic-tool protocol; E3 tools route only to the
    // explicit binding; E4 create/current-CAS succeed, stale/read-only writes
    // fail without clobbering; E5 a policy-only WebFetch does not manufacture a
    // capability or materialize a Sandbox. Decision rules M1=C1+C2+C3 -> E1..E3;
    // M2=C4 absent -> create; M3=C4 current -> update; M4=C4 stale -> reject;
    // M5=read-only binding -> reject; M6 a cold Runtime rebuild retains the
    // physical Session's frozen delivery and tool projection; M7=C1+C2+
    // unpublished WebFetch -> E5. Constraint K1 a rebuild may replace only
    // process-local Runtime state, never the Session-owned delivery decision.
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };
    use awaken_runtime_contract::tool::ToolCall;

    struct SemanticMemoryModel {
        fetch_url: Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl LlmExecutor for SemanticMemoryModel {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            let last = request.messages.last().expect("semantic Memory message");
            let output = if last.role == Role::Tool {
                AssistantOutput::text(format!(
                    "runtime-semantic-tool:{}",
                    block_text(&last.content)
                ))
            } else if let Some(url) = self.fetch_url.lock().unwrap().clone() {
                AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                    call_id: "runtime-web-fetch".into(),
                    tool_id: "web_fetch".into(),
                    arguments: serde_json::json!({ "url": url }),
                }])
            } else {
                AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                    call_id: "runtime-read-memory".into(),
                    tool_id: "read_memory".into(),
                    arguments: serde_json::json!({
                        "binding": "test-input-1",
                        "path": "/preference.md"
                    }),
                }])
            };
            Ok(ChatResponse {
                output,
                usage: None,
                stop_reason: None,
            })
        }
    }

    let model = Arc::new(SemanticMemoryModel {
        fetch_url: Mutex::new(None),
    });
    let host = Arc::new(SharedHost::new(model.clone(), "stub"));
    let managed = managed_with_resource_source(host.clone());
    let store_a = "semantic-store-a";
    let store_b = "semantic-store-b";
    host.memory_repository()
        .create(store_b, "/preference.md", "from-b")
        .await
        .unwrap();

    let mut init = bare_session("assistant", host.local_workspace());
    init.environment = on_tool_use_environment();
    init.tools = Some(
        awaken_session_configuration_contract::SessionToolConfiguration {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy {
                    enabled: false,
                    permission: ToolPermissionRequirement::AlwaysAllow,
                },
                overrides: ["web_fetch", "web_search"]
                    .into_iter()
                    .map(|name| {
                        ToolPolicyOverride::new(
                            name,
                            ToolExecutionPolicy {
                                enabled: true,
                                permission: ToolPermissionRequirement::AlwaysAllow,
                            },
                        )
                    })
                    .collect(),
            }],
            client_tools: Vec::new(),
        },
    );
    init.resources = effective_resources(vec![
        TestInput {
            kind: "memory_store".into(),
            id: store_a.into(),
            mount_path: "memory/a".into(),
            access: ResourceAccess::ReadWrite,
            instructions: Some("Use for durable project facts.".into()),
            initial_branch: None,
            initial_commit: None,
        },
        TestInput {
            kind: "memory_store".into(),
            id: store_b.into(),
            mount_path: "memory/b".into(),
            access: ResourceAccess::ReadOnly,
            instructions: Some("Use for user preferences.".into()),
            initial_branch: None,
            initial_commit: None,
        },
    ]);
    managed
        .install_test_session_init("semantic-memory", init)
        .await
        .unwrap();
    let context = host
        .ctx_for("semantic-memory", Some("assistant"))
        .await
        .unwrap();

    assert!(
        host.session_environment("semantic-memory").await.is_none(),
        "M1/E1"
    );
    assert!(
        host.sandbox_spec("semantic-memory").mounts.is_empty(),
        "M1/E1"
    );
    let prompt = host.thread_session_prompts("semantic-memory").join("\n");
    assert!(
        prompt.contains("test-input-0") && prompt.contains("test-input-1"),
        "M1/E2"
    );
    assert!(prompt.contains("through the memory tools"), "M1/E2");
    assert!(!prompt.contains("is mounted"), "M1/E2");
    let misplaced_file_call = context
        .attempt_context
        .tool_executor
        .as_ref()
        .expect("filesystem-free executor")
        .invoke(&ToolCall {
            call_id: "misplaced-read".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"path": "anything"}),
        })
        .await
        .unwrap_err();
    assert!(
        misplaced_file_call
            .to_string()
            .contains("filesystem-free Session"),
        "M1/E1 fail closed"
    );
    assert!(
        host.session_environment("semantic-memory").await.is_none(),
        "M1/E1 rejected file call cannot awaken Sandbox"
    );
    assert!(
        context
            .config
            .resolved_spec
            .tool_descriptors
            .iter()
            .any(|descriptor| descriptor.id == "read_memory"),
        "M1/E3 model surface"
    );
    let runtime_read = run_prepared_session_messages(
        &managed,
        "assistant",
        "semantic-memory",
        vec![Message::text(
            MessageId("semantic-memory-runtime-read".into()),
            Role::User,
            "Read the preference memory",
        )],
    )
    .await
    .expect("M1 final Runtime dispatches read_memory");
    assert!(
        runtime_read
            .new_messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .map(|message| block_text(&message.content))
            .any(|text| text.contains("runtime-semantic-tool") && text.contains("from-b")),
        "M1/E3 model descriptor reaches the bound executor through host.run"
    );
    assert!(
        host.session_environment("semantic-memory").await.is_none(),
        "M1/E1 semantic Runtime execution remains Sandbox-free"
    );

    *model.fetch_url.lock().unwrap() = Some("https://fixture.invalid/value".into());
    let runtime_fetch = run_prepared_session_messages(
        &managed,
        "assistant",
        "semantic-memory",
        vec![Message::text(
            MessageId("semantic-memory-runtime-fetch".into()),
            Role::User,
            "Fetch the configured URL",
        )],
    )
    .await
    .expect("M7 unpublished WebFetch fails within the Runtime tool result");
    assert!(
        runtime_fetch
            .new_messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .map(|message| block_text(&message.content))
            .any(|text| text.contains("runtime-semantic-tool:unknown tool: web_fetch")),
        "M7/E5 tool policy cannot manufacture WebFetch: {:?}",
        runtime_fetch.new_messages
    );
    assert!(
        host.session_environment("semantic-memory").await.is_none(),
        "M7/E5 rejected WebFetch remains Sandbox-free"
    );

    let bindings = host
        .session_slots
        .read("semantic-memory", |slot| slot.memory_bindings.clone())
        .unwrap();
    let tools = crate::session_memory_tools::SessionMemoryTools::new(bindings).unwrap();
    let invoke = |tool_id: &str, arguments: serde_json::Value| {
        let tool = tools
            .executors
            .iter()
            .find(|tool| tool.id() == tool_id)
            .cloned()
            .expect("semantic Memory tool is wired");
        let tool_id = tool_id.to_string();
        async move {
            tool.invoke(ToolCall {
                call_id: format!("{tool_id}-call"),
                tool_id,
                arguments,
            })
            .await
        }
    };
    let read_b = invoke(
        "read_memory",
        serde_json::json!({"binding": "test-input-1", "path": "/preference.md"}),
    )
    .await
    .unwrap();
    assert!(
        !read_b.is_error && read_b.text().contains("from-b"),
        "M1/E3"
    );

    let created = invoke(
        "write_memory",
        serde_json::json!({"binding": "test-input-0", "path": "/fact.md", "content": "v1"}),
    )
    .await
    .unwrap();
    assert!(!created.is_error, "M2/E4");
    let created: awaken_resource_contract::Memory = serde_json::from_str(&created.text()).unwrap();
    let updated = invoke(
        "write_memory",
        serde_json::json!({
            "binding": "test-input-0",
            "path": "/fact.md",
            "content": "v2",
            "expected_generation": created.generation()
        }),
    )
    .await
    .unwrap();
    assert!(!updated.is_error, "M3/E4");
    let stale = invoke(
        "write_memory",
        serde_json::json!({
            "binding": "test-input-0",
            "path": "/fact.md",
            "content": "clobber",
            "expected_generation": {
                "memory_id": created.id,
                "version": created.version,
                "content_sha256": "stale"
            }
        }),
    )
    .await
    .unwrap_err();
    assert!(stale.to_string().contains("cas conflict"), "M4/E4: {stale}");
    let read_only = invoke(
        "write_memory",
        serde_json::json!({"binding": "test-input-1", "path": "/new.md", "content": "no"}),
    )
    .await
    .unwrap_err();
    assert!(
        read_only.to_string().contains("read-only"),
        "M5/E4: {read_only}"
    );
    assert_eq!(
        host.memory_repository()
            .get_by_path(store_a, "/fact.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v2"),
        "M4 never clobbers"
    );
    host.session_slots.update("semantic-memory", |slot| {
        slot.runtime = None;
    });
    let rebuilt = host
        .ctx_for("semantic-memory", Some("assistant"))
        .await
        .expect("M6 rebuilds from the frozen Session projection");
    assert_eq!(
        host.session_slots
            .read("semantic-memory", |slot| slot.content_delivery)
            .flatten(),
        Some(crate::session_slot::ManagedContentDelivery::SemanticTools),
        "M6 keeps one delivery path"
    );
    assert!(
        rebuilt
            .config
            .resolved_spec
            .tool_descriptors
            .iter()
            .any(|descriptor| descriptor.id == "read_memory"),
        "M6 preserves the Session-owned semantic Memory tools"
    );
    assert!(
        host.session_environment("semantic-memory").await.is_none(),
        "M6 does not materialize a Sandbox while rebuilding"
    );
}

#[tokio::test]
async fn activation_applies_current_resource_state_as_a_deny_only_overlay() {
    use awaken_resource_contract::{
        ChangeMemoryStoreState, ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition,
        RegisterMemoryStore, ResourceAdministration as _, ResourceState,
    };

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = test_memory_store_id();
    let catalog = resource_registry();
    catalog
        .register_memory_store(RegisterMemoryStore {
            definition: MemoryStoreDefinition {
                id: store_id.clone().into(),
                workspace_id: host.local_workspace().into(),
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
        .expect("register test MemoryStore");
    let manifest = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    catalog
        .change_memory_store_state(ChangeMemoryStoreState {
            workspace_id: host.local_workspace().into(),
            id: store_id.clone().into(),
            state: ResourceState::Suspended,
        })
        .await
        .expect("suspend test MemoryStore");
    let managed = crate::ManagedHost::new(host.clone()).with_resource_validator(catalog.clone());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = manifest;

    let error = managed
        .install_test_session_init("t-suspended", init)
        .await
        .unwrap_err();

    assert!(error.message.contains("not active"));
    assert!(host.sandbox_spec("t-suspended").mounts.is_empty());
}

#[tokio::test]
async fn memory_activation_enforces_catalog_workspace_without_iam_policy_logic() {
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RegisterMemoryStore,
        ResourceAdministration as _, ResourceState,
    };

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = test_memory_store_id();
    let catalog = resource_registry();
    catalog
        .register_memory_store(RegisterMemoryStore {
            definition: MemoryStoreDefinition {
                id: store_id.clone().into(),
                workspace_id: "workspace-a".into(),
                name: "private-memory".into(),
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
        .expect("register workspace-fenced MemoryStore");
    let managed = crate::ManagedHost::new(host.clone()).with_resource_validator(catalog);
    let mut init = bare_session("agent", "workspace-b");
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    let error = managed
        .install_test_session_init("wrong-workspace", init)
        .await
        .unwrap_err();
    assert!(error.message.contains("not found"));
    assert!(host.sandbox_spec("wrong-workspace").mounts.is_empty());
    assert!(host.memory_for_thread("wrong-workspace").is_none());
}

/// The same effective input contract realizes File and Repository resources without
/// exposing their authoring repository to Runtime.
#[tokio::test]
async fn install_test_session_init_mounts_effective_file_and_stages_effective_repo() {
    use awaken_provisioning_contract::{MountAccess, MountSource};
    use awaken_session_configuration_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));

    // Seed a file blob and pass the already-resolved File and Repository inputs.
    let binary = vec![0, 0xff, b'R', 0x80, b'\n'];
    let record = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            host.local_workspace(),
            "notes.txt".into(),
            "application/octet-stream".into(),
            &binary,
        )
        .await
        .unwrap();
    let file_id = record.id;
    let managed = managed_with_resource_source(host.clone());

    managed
        .install_test_session_init(
            "t-multi",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: effective_resources(vec![
                    TestInput {
                        kind: "file".into(),
                        id: file_id.clone(),
                        mount_path: "/mnt/files/notes.txt".into(),
                        access: ResourceAccess::ReadOnly,
                        instructions: None,
                        initial_branch: None,
                        initial_commit: None,
                    },
                    TestInput {
                        kind: "github_repository".into(),
                        id: "https://github.com/awaken/example.git".into(),
                        mount_path: "/workspace/repo".into(),
                        access: ResourceAccess::ReadOnly,
                        instructions: None,
                        initial_branch: None,
                        initial_commit: None,
                    },
                ]),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .unwrap();

    // The exact bytes and their content identity cross the neutral mount contract;
    // no UTF-8 conversion can corrupt binary input.
    let spec = host.sandbox_spec("t-multi");
    let mount = &spec.mounts[0];
    assert_eq!(mount.mount_path, "/mnt/session/uploads/mnt/files/notes.txt");
    assert_eq!(mount.access, MountAccess::ReadOnly);
    let MountSource::InlineBytes {
        contents,
        content_hash,
    } = &mount.source
    else {
        panic!("effective File input must use the binary-safe carried source")
    };
    assert_eq!(contents, &binary);
    assert_eq!(content_hash.as_deref(), Some(record.blob_id.as_str()));

    // The repo is staged for a host-side clone (not a byte mount).
    let repositories = host.thread_repository_activations("t-multi");
    assert_eq!(
        repositories.len(),
        1,
        "the bound repository has one realization plan"
    );
    assert_eq!(
        repositories[0].plan.source_remote_url,
        "https://github.com/awaken/example.git"
    );
    assert_eq!(
        repositories[0].plan.transport_url,
        "https://github.com/awaken/example.git"
    );
    assert_eq!(
        repositories[0].plan.mount_path, "/workspace/repo",
        "the realization plan carries the same Agent-visible path as the frozen input"
    );
}

/// Repository mount-path cause/effect rule: C1 a new command names the canonical
/// `/workspace/repo` child; C2 no Environment exists yet. C1+C2 stages exactly
/// one Repository activation without relocating the path, and keeps the active
/// manifest absent until the physical transition completes.
#[tokio::test]
async fn complete_projection_preserves_the_canonical_repository_path_without_relocation() {
    use awaken_session_configuration_contract::SessionInit;

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let verifier = Arc::new(FixedRepositoryTransport(
        awaken_resource_contract::RepositoryTransport::Direct,
    ));
    let managed = managed_with_resource_source(host.clone())
        .with_repository_binding_verifier(verifier)
        .install_dispatch_session_runtime();
    let resources = effective_resources(vec![TestInput {
        kind: "github_repository".into(),
        id: "https://github.com/awaken/example.git".into(),
        mount_path: "/workspace/repo".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    managed
        .install_complete_test_session(
            "canonical-repository-path",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: resources.clone(),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .expect("canonical Repository path stages through the complete projection");
    managed
        .environment()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "EnvironmentRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .apply_session_inputs(
            "canonical-repository-path",
            &resource_transition(host.local_workspace(), 0, Default::default(), 0, resources),
        )
        .await
        .expect("compile the exact cold Repository transition");
    let activations = host.thread_repository_activations("canonical-repository-path");
    assert_eq!(activations.len(), 1, "one exact Repository activation");
    assert_eq!(activations[0].plan.mount_path, "/workspace/repo");
    assert!(
        host.thread_resource_manifest("canonical-repository-path")
            .is_none(),
        "staging is not physical completion"
    );
    assert!(
        host.sandbox_spec("canonical-repository-path")
            .mounts
            .is_empty(),
        "Repository stays on its sole activation path"
    );
}

/// Terminal publication cause/effect decision table:
///
/// | Rule | frozen input | local branch/HEAD | remote ref | Effect |
/// |---|---|---|---|---|
/// | P1 | one writable Repository | exact expected coordinate | absent | publish and return canonical Session receipt |
/// | P2 | same command replay | exact expected coordinate | same commit | no-op with byte-identical receipt |
/// | P3 | complete ordinary projection carries empty rev0 -> desired rev1 | exact | any | canonical Dispatch install owns baseline plus Resource staging before realization; publication performs no second lookup |
/// | P4 | replacement Host starts without root/child slots and claims one Resident aggregate binding | exact | stale without an authorized prior | cold install/adopt, child preparation, durable rejection, root preparation, then physical disposal |
///
/// Invalid coordinate and mismatched remote rules are exhaustively covered at
/// the Repository realizer boundary; this integration test proves the
/// ManagedHost uses that sole implementation and retains the Environment until
/// first execution, replay, and the ordered terminal reconciliation have
/// produced evidence.
#[tokio::test]
async fn managed_terminal_repository_publication_pushes_exact_commit_and_replays() {
    for retained_terminal in [false, true] {
        terminal_repository_publication_fixture(retained_terminal, false).await;
    }
}

#[tokio::test]
async fn terminal_repository_publication_closed_source_converges_without_git() {
    // S1 the exact aggregate binding and current lease remain valid, but the
    // provider observes the test-owned filesystem has disappeared before Git.
    // E1 return a durable negative publication outcome, never a success or an
    // invented remote-ref conflict; E2 leave the independent remote unchanged.
    // Ready/replay/foreign-retirement rows reuse this same production fixture.
    // This is source-closure evidence, not proof of an earlier remote effect.
    terminal_repository_publication_fixture(false, true).await;
}

async fn terminal_repository_publication_fixture(retained_terminal: bool, source_closed: bool) {
    let temp = tempfile::tempdir().expect("publication fixture");
    let sandbox_root = temp.path().join("sandboxes");
    let namespace_probe = awaken_sandbox_local::NamespaceProvider::new(&sandbox_root);
    if awaken_provisioning_contract::SandboxProvider::probe_ready(&namespace_probe)
        .await
        .is_err()
    {
        eprintln!("skipping Namespace execution assertion: OS sandbox/user namespaces unavailable");
        return;
    }
    let remote = temp.path().join("remote.git");
    let seed = temp.path().join("seed");
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let status = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .expect("git fixture command");
        assert!(status.success(), "git {args:?}");
    };
    git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
    git(
        temp.path(),
        &["clone", remote.to_str().unwrap(), seed.to_str().unwrap()],
    );
    git(&seed, &["config", "user.name", "seed"]);
    git(&seed, &["config", "user.email", "seed@example.invalid"]);
    std::fs::write(seed.join("README.md"), "base").expect("seed file");
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-m", "base"]);
    git(&seed, &["push", "-u", "origin", "HEAD"]);

    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let mut raw_host =
        SharedHost::new(Arc::new(OkModel), "stub").with_session_recovery_control(control.clone());
    raw_host.env.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            sandbox_root.clone(),
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let repository_path_fidelity = raw_host.env.session_provider.capabilities().path_fidelity;
    let host = Arc::new(raw_host);
    let workspace_id = host.local_workspace().to_string();
    let managed = managed_with_resource_source(host.clone());
    let session_id = "terminal-publication-local";
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "terminal-publication-worker".into(),
        runtime_incarnation: "terminal-publication-worker:incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
            .saturating_add(60_000),
    };
    // Reuse the Environment-binding test authority so the exact realization
    // lease produces a V2 provider fence and owned-path evidence. P1 therefore
    // exercises the canonical create/persist/publish path instead of a legacy
    // unfenced test Sandbox.
    let initial_binding_sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    managed
        .realization()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(initial_binding_sink.clone());
    let resources = effective_repository(
        "repository-publication",
        remote.to_str().unwrap(),
        "/workspace/repository",
        None,
    );
    let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment: session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
            runtime_placement:
                awaken_session_configuration_contract::SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model_override: None,
            model: "stub".into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    let ordinary_projection = awaken_session_realization_contract::FrozenSessionProjection {
        workspace_id: workspace_id.clone(),
        revision: awaken_session_types::SessionRevision(1),
        baseline,
        agent_publication: None,
        environment: Default::default(),
        resource_revision: 1,
        resources: resources.clone(),
        previous_resource_manifest: Some(
            awaken_session_resource_contract::SessionResourceManifest::at_revision(
                &workspace_id,
                0,
                Default::default(),
            ),
        ),
        tools: Default::default(),
        mcp: Vec::new(),
        request_context: Vec::new(),
    };
    // P1/P3 cause-effect rule: a cold Session with desired revision 1 requires
    // both its immutable baseline and exact empty-rev0 -> desired-rev1 Resource
    // transition before any Environment can exist. The complete Runtime port
    // owns both effects; no test-only preparation or direct slot write may
    // create a competing partial projection.
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &managed,
        session_id,
        ordinary_projection.clone(),
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("P1/P3 install complete ordinary projection before realization");
    host.install_session_realization_lease(session_id, lease.clone())
        .unwrap();
    let realization = run_prepared_session_messages(
        &managed,
        "agent",
        session_id,
        vec![Message::text(
            MessageId("terminal-publication-input".into()),
            Role::User,
            "prepare repository",
        )],
    )
    .await;
    if !repository_path_fidelity {
        let error = match realization {
            Err(error) => error,
            Ok(_) => panic!(
                "a Namespace provider without path fidelity must reject Repository publication"
            ),
        };
        assert!(
            error
                .message
                .contains("one sandbox-absolute workspace path"),
            "P0 capability rejection must explain the shared-path invariant: {}",
            error.message
        );
        assert!(
            host.session_environment(session_id).await.is_none(),
            "P0 rejection happens before a publishable environment is exposed"
        );
        let published = std::process::Command::new("git")
            .current_dir(&remote)
            .args(["show-ref", "--verify", "--quiet", "refs/heads/awf/work"])
            .status()
            .expect("inspect publication ref");
        assert!(
            !published.success(),
            "P0 rejected realization cannot publish a remote branch"
        );
        return;
    }
    realization.expect("P1 realize Session Environment");
    let environment = host
        .session_environment(session_id)
        .await
        .expect("P1 retained Environment");
    let status = environment
        .run_test_command(awaken_provisioning_contract::Command::new([
            "sh",
            "-c",
            concat!(
                "git -C repository config user.name agent && ",
                "git -C repository config user.email agent@example.invalid && ",
                "git -C repository checkout -b awf/work && ",
                "printf changed > repository/README.md && ",
                "git -C repository add README.md && ",
                "git -C repository commit -m changed && ",
                "mkdir -p .terminal-publication-proof && ",
                "git -C repository rev-parse HEAD > .terminal-publication-proof/commit.txt"
            ),
        ]))
        .await
        .expect("P1 author commit");
    assert_eq!(status.code, Some(0), "P1 exact local commit");
    let commit = environment
        // P1 observes only the dedicated proof directory. Traversing the
        // attached Repository would mix its provider-owned Git metadata into
        // this assertion and duplicate the Repository realizer's own checks.
        .list_workspace_files(".terminal-publication-proof")
        .await
        .expect("P1 list workspace")
        .into_iter()
        .find_map(|(path, bytes)| (path == "commit.txt").then_some(bytes))
        .map(String::from_utf8)
        .transpose()
        .expect("P1 UTF-8 commit")
        .expect("P1 commit file")
        .trim()
        .to_string();
    let intent = awaken_session_state_contract::SessionRepositoryPublicationIntent {
        input: resources.inputs()[0].clone(),
        expectation: awaken_provisioning_contract::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: commit.clone(),
            expected_prior_commit: None,
        },
    };
    let child_id = "terminal-publication-child";
    let mut publication_ready = terminal_test_session(
        session_id,
        &ordinary_projection,
        lease.clone(),
        [child_id.to_string()],
        Some(intent.clone()),
    );
    let child_cleanup = publication_ready
        .terminal_cleanup
        .command_for(&awaken_session_types::SessionId::from(session_id), child_id)
        .expect("P4 child cleanup command");
    let child_effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        child_cleanup.clone(),
        lease.clone(),
    );
    publication_ready
        .record_terminal_cleanup_preparation(
            &ordinary_projection.workspace_id,
            &lease,
            awaken_session_state_contract::SessionCleanupPreparation::try_new(
                &child_effect,
                child_effect.sandbox_effect_fence().unwrap(),
                Vec::new(),
            )
            .unwrap(),
            None,
        )
        .expect("P4 aggregate admits durable child preparation before publication");
    let command = publication_ready
        .terminal_cleanup
        .publication_command(&awaken_session_types::SessionId::from(session_id))
        .expect("P1 command projection")
        .expect("P1 command");
    let mut terminal_projection = ordinary_projection.clone();
    let retained = host
        .session_slots
        .read(session_id, |slot| {
            slot.environment_owner.terminal_bound_environment()
        })
        .flatten()
        .expect("P1 retained exact Environment identity");
    let crate::session_slot::BoundSessionEnvironmentIdentity::Durable {
        effect_id,
        generation,
    } = retained.identity
    else {
        panic!("P1 Environment must retain its generated Store-read authority");
    };
    // A changed aggregate Environment is a new root-CAS snapshot.
    terminal_projection.revision.0 += 1;
    terminal_projection.environment =
        awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: retained.binding,
            effect_id: Some(effect_id),
            generation: Some(generation),
            idle_since_unix_ms: None,
        };
    terminal_projection.previous_resource_manifest = Some(
        awaken_session_resource_contract::SessionResourceManifest::at_revision(
            &workspace_id,
            1,
            resources.clone(),
        ),
    );
    awaken_session_execution_contract::TerminalCleanupRuntime::install_terminal_cleanup_assignment(
        &managed,
        &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            session_id: session_id.into(),
            projection: terminal_projection.clone(),
            authority: crate::session_slot::test_lease_receipt(lease.clone()),
        },
        crate::session_slot::test_observed_lease(lease.clone()),
    )
    .await
    .expect("P1 install the aggregate-fenced publication assignment");
    if source_closed {
        // Inject actual loss of only this test's sole owned source after all
        // authoring commands joined. Preserve sibling provider markers: deleting
        // identity evidence is Incompatible, not an absence proof. Observe the
        // sole physical directory instead of copying the provider name codec.
        let sources = std::fs::read_dir(&sandbox_root)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .filter(|entry| {
                std::fs::read_to_string(
                    entry
                        .path()
                        .join(
                            awaken_provisioning_contract::WorkspaceLayout::ROOT
                                .trim_start_matches('/'),
                        )
                        .join(".terminal-publication-proof/commit.txt"),
                )
                .is_ok_and(|observed| observed.trim() == commit)
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert_eq!(sources.len(), 1, "S1 one fixture-owned realization");
        std::fs::remove_dir_all(&sources[0]).expect("S1 remove owned fixture source");
        let outcome = managed
            .terminal_cleanup()
            .unwrap()
            .execute_terminal_repository_publication_for_lease(command, &lease)
            .await
            .expect("S1 exact closed source must return a negative outcome");
        assert!(
            matches!(
                outcome,
                awaken_session_state_contract::SessionRepositoryPublicationEffect::Rejected(_)
            ),
            "S1 source loss cannot publish a commit"
        );
        let published = std::process::Command::new("git")
            .current_dir(&remote)
            .args(["show-ref", "--verify", "--quiet", "refs/heads/awf/work"])
            .status()
            .expect("S1 independent remote observation");
        assert!(!published.success(), "S1 remote remains unchanged");
        return;
    }
    if retained_terminal {
        // P5: a previous terminal preparation returned no aggregate receipt but
        // retained this exact bound Arc behind retirement. Provider remains Ready;
        // publication must reuse it under the current terminal assignment, without
        // reopening ordinary execution. A cold Host does not reproduce this shape.
        host.session_slots.update(session_id, |slot| {
            let owned = slot.environment_owner.terminal_bound_environment().unwrap();
            slot.environment_owner = crate::session_slot::SessionEnvironmentOwner::Retiring(
                crate::session_slot::RetiringSessionEnvironment {
                    cause: crate::session_slot::SessionEnvironmentRetirementCause::Terminal {
                        effect_id: "terminal-source-preparation".into(),
                    },
                    owned: crate::session_slot::RetiringEnvironmentOwner::Bound(owned),
                },
            );
            slot.runtime = None;
        });
        assert!(
            host.session_environment(session_id).await.is_none(),
            "P5 ordinary execution remains closed"
        );
        // P6: the generic live/checkpoint source path remains denied, even with
        // an exact handle and current lease. P7: exact Durable revocation with
        // Ready physical source + a current terminal effect must replace the
        // closed wrapper through existing rebind/Candidate retirement. It must
        // preserve identity/data and stay invisible to ordinary execution;
        // simply allowing the old Arc or publishing a Resident fails P7.
        let binding = terminal_projection.environment.binding().unwrap();
        let publication_fence = lease
            .sandbox_effect_fence(command.effect_id.clone())
            .unwrap();
        let ordinary = host
            .prepare_bound_environment_for_effect_under_lifecycle(
                session_id,
                binding,
                &publication_fence,
                Some(&resources),
                crate::host::BoundEnvironmentPreparationMode::LiveSource,
            )
            .await;
        assert!(
            ordinary.is_err(),
            "P6 terminal retirement remains hidden from ordinary live IO"
        );
        host.session_slots.update(session_id, |slot| {
            if let crate::session_slot::SessionEnvironmentOwner::Retiring(retiring) =
                &mut slot.environment_owner
            {
                retiring.cause =
                    crate::session_slot::SessionEnvironmentRetirementCause::RealizationRevocation;
            } else {
                panic!("P7 retained owner");
            }
        });
        let before = host
            .session_slots
            .read(session_id, |slot| {
                slot.environment_owner.terminal_bound_environment().unwrap()
            })
            .unwrap();
        managed
            .terminal_cleanup()
            .unwrap()
            .execute_terminal_repository_publication_for_lease(command.clone(), &lease)
            .await
            .expect("P7 fresh wrapper resumes exact terminal publication");
        let after = host
            .session_slots
            .read(session_id, |slot| {
                assert!(
                    matches!(
                        slot.environment_owner,
                        crate::session_slot::SessionEnvironmentOwner::Retiring(_)
                    ),
                    "P7 not Resident"
                );
                slot.environment_owner.terminal_bound_environment().unwrap()
            })
            .unwrap();
        assert!(
            !Arc::ptr_eq(&before.environment, &after.environment),
            "P7 no old wrapper reuse"
        );
        assert_eq!(before.binding, after.binding, "P7 exact durable binding");
        assert_eq!(
            before.identity, after.identity,
            "P7 original Store identity"
        );
        assert_eq!(
            before.environment.handle(),
            after.environment.handle(),
            "P7 exact physical target"
        );
        assert!(
            host.session_environment(session_id).await.is_none(),
            "P7 ordinary execution stays closed"
        );
    }
    let first = managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .execute_terminal_repository_publication_for_lease(command.clone(), &lease)
        .await
        .expect("P1 publish");
    let replay = managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .execute_terminal_repository_publication_for_lease(command.clone(), &lease)
        .await
        .expect("P2 replay");
    assert_eq!(first, replay, "P2 canonical first/replay receipt");
    let awaken_session_state_contract::SessionRepositoryPublicationEffect::Published(first) = first
    else {
        panic!("P1 expected a publication receipt");
    };
    assert_eq!(first.effect_receipt.commit, commit, "P1 exact receipt");
    let remote_commit = std::process::Command::new("git")
        .current_dir(temp.path())
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/awf/work",
        ])
        .output()
        .expect("P1 inspect remote");
    assert!(remote_commit.status.success(), "P1 remote branch exists");
    assert_eq!(
        String::from_utf8(remote_commit.stdout)
            .expect("P1 remote commit UTF-8")
            .trim(),
        commit,
        "P1 remote exact commit"
    );
    let divergent = temp.path().join("divergent");
    git(
        temp.path(),
        &[
            "clone",
            "--branch",
            "awf/work",
            remote.to_str().unwrap(),
            divergent.to_str().unwrap(),
        ],
    );
    git(&divergent, &["config", "user.name", "remote-writer"]);
    git(
        &divergent,
        &["config", "user.email", "remote@example.invalid"],
    );
    std::fs::write(divergent.join("README.md"), "remote changed").unwrap();
    git(&divergent, &["commit", "-am", "remote changed"]);
    git(&divergent, &["push", "origin", "awf/work"]);
    assert_eq!(
        host.session_environment(session_id).await.is_some(),
        !retained_terminal,
        "P1-P2/P5 publication preserves ordinary residency or retirement",
    );
    assert!(
        host.session_slots
            .read(session_id, |slot| {
                slot.environment_owner
                    .terminal_bound_environment()
                    .is_some()
            })
            .unwrap_or(false),
        "P1-P2/P5 exact physical source survives until root cleanup"
    );

    let control_session = terminal_test_session(
        session_id,
        &terminal_projection,
        lease.clone(),
        [child_id.to_string()],
        Some(intent),
    );
    assert_eq!(
        control_session
            .terminal_cleanup
            .command_for(&awaken_session_types::SessionId::from(session_id), child_id),
        Some(child_cleanup),
        "P4 Control consumes the same aggregate-derived child command",
    );
    assert_eq!(
        control_session
            .terminal_cleanup
            .publication_command(&awaken_session_types::SessionId::from(session_id))
            .expect("P4 aggregate-owned Repository publication projection"),
        None,
        "P4 fresh aggregate keeps publication behind child preparation",
    );
    control.assignments.lock().unwrap().push_back(
        awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            session_id: session_id.into(),
            projection: terminal_projection,
            authority: crate::session_slot::test_lease_receipt(lease.clone()),
        },
    );
    *control.session.lock().unwrap() = Some(control_session);
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: lease.owner.clone(),
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_expires_at_unix_ms: lease.expires_at_unix_ms,
        reassign_existing_lease: false,
    };

    // P4 cause/effect graph: C1 the old Worker durably published one exact
    // Resident binding and then lost all process-local state; C2 a replacement
    // Host has the same Namespace root, local Workspace, and Control authority
    // but no root/child slot; C3 claim-next returns the sole aggregate-frozen
    // assignment; C4 child preparation is still pending; C5 the frozen
    // create-only publication has no authorized prior while the remote advanced
    // after P1/P2. Effects: E1 only the canonical cold
    // claim/install/driver path may reconstruct the root; E2 live publication
    // adoption writes no ordinary Environment receipt and leaves the provider
    // marker Ready, so only the later root preparation enters terminal takeover
    // under T; E3 child preparation precedes the durable CAS rejection, root
    // preparation, and disposal; E4 all replacement slots and Environment
    // handles are retired. Missing, foreign,
    // Disposing, or otherwise non-live provider evidence remains owned by the
    // adjacent fail-closed recovery tests.
    //
    // | Rule | durable source | replacement slots | claim | Effect |
    // |---|---|---|---|---|
    // | P4a | Resident exact | root/child absent | exact | E1 + E2 + E3 + E4 |
    // | P4b | missing/foreign/Disposing | root/child absent | exact | fail closed; no receipt/delete |
    drop(environment);
    drop(managed);
    drop(host);

    let mut raw_replacement = SharedHost::new(Arc::new(OkModel), "stub")
        .with_session_recovery_control(control.clone())
        .with_local_workspace(workspace_id);
    raw_replacement.env.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            sandbox_root,
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let replacement_host = Arc::new(raw_replacement);
    let replacement_managed = managed_with_resource_source(replacement_host.clone());
    let replacement_binding_sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&replacement_host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: AtomicBool::new(false),
        fail: false,
        require_realization: true,
        owned_session_id: None,
        committed_environment: None,
    });
    replacement_managed
        .realization()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(replacement_binding_sink.clone());

    assert!(
        !replacement_host.session_slots.contains(session_id)
            && !replacement_host.session_slots.contains(child_id),
        "P4 replacement starts without root or child process-local slots"
    );
    assert!(
        replacement_host
            .session_environment(session_id)
            .await
            .is_none(),
        "P4 replacement starts without a resident Environment wrapper"
    );

    assert_eq!(
        replacement_host
            .recover_terminal_cleanup_assignments(target.clone())
            .await
            .expect("P4 cold assignment recovery"),
        1,
        "P4 the replacement claims and drives exactly one assignment"
    );
    assert_eq!(
        control.claim_targets.lock().unwrap().as_slice(),
        [target.clone(), target],
        "P4 one exact claim plus the terminating empty scan"
    );
    assert_eq!(
        control.events.lock().unwrap().as_slice(),
        [
            "cleanup:poll",
            "cleanup:poll",
            "cleanup:prepared:terminal-publication-child",
            "cleanup:poll",
            "publication:poll",
            "publication:rejection",
            "cleanup:poll",
            "cleanup:prepared:terminal-publication-local",
            "cleanup:poll",
            "cleanup:disposed",
        ],
        "P4 child preparation -> durable publication rejection -> root preparation -> physical disposal"
    );
    assert!(
        control.publication_receipts.lock().unwrap().is_empty(),
        "P4"
    );
    assert_eq!(
        control.publication_rejections.lock().unwrap().len(),
        1,
        "P4"
    );
    assert_eq!(control.preparations.lock().unwrap().len(), 2, "P4");
    assert_eq!(control.disposals.lock().unwrap().len(), 1, "P4");
    assert_eq!(
        replacement_binding_sink
            .authorize_calls
            .load(Ordering::SeqCst),
        0,
        "P4 terminal adoption never enters ordinary binding authorization"
    );
    assert_eq!(
        replacement_binding_sink.calls.load(Ordering::SeqCst),
        0,
        "P4 terminal adoption writes no ordinary binding receipt"
    );
    assert!(
        replacement_host
            .session_environment(session_id)
            .await
            .is_none(),
        "P4 physical disposal runs only after publication and both durable preparations"
    );
    assert!(
        !replacement_host.session_slots.contains(session_id)
            && !replacement_host.session_slots.contains(child_id),
        "P4 completion retires replacement root and child slots"
    );
}

#[tokio::test]
async fn file_activation_rejects_bytes_that_do_not_match_the_file_id() {
    // Cause/effect decision rule D1: the authoritative File application returns
    // bytes whose canonical digest differs from the logical record's blob id ->
    // staging fails closed before a Sandbox mount is published.
    use awaken_file_store::{FileStore, FileStoreError};

    struct CorruptFileStore;

    #[async_trait::async_trait]
    impl FileStore for CorruptFileStore {
        async fn put(&self, _bytes: &[u8]) -> Result<String, FileStoreError> {
            unreachable!("test only reads the corrupt entry")
        }

        async fn get(&self, _id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
            Ok(Some(b"different bytes".to_vec()))
        }

        async fn list(&self) -> Result<Vec<String>, FileStoreError> {
            Ok(Vec::new())
        }

        async fn delete(&self, _id: &str) -> Result<bool, FileStoreError> {
            Ok(false)
        }
    }

    let declared_blob_id = awaken_sandbox_local::content_fingerprint(b"declared bytes");
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub");
    let corrupt_store = Arc::new(CorruptFileStore);
    raw_host.resources.file_store = corrupt_store.clone();
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        corrupt_store,
        raw_host.resources.file_catalog.clone(),
        raw_host
            .resource_reclamation()
            .expect("test lifecycle repository"),
        system_clock(),
    ));
    raw_host = raw_host.with_file_application(
        application.clone(),
        Arc::new(
            awaken_resource_application::ApplicationFileContentSource::new(application.clone()),
        ),
        Arc::new(awaken_resource_application::ApplicationArtifactPublisher::new(application)),
    );
    let host = Arc::new(raw_host);
    let public_id = "file_corrupt".to_string();
    host.file_catalog()
        .create_file(awaken_resource_contract::FileRecord {
            id: public_id.clone(),
            workspace_id: host.local_workspace().into(),
            blob_id: declared_blob_id,
            filename: "input.bin".into(),
            mime_type: "application/octet-stream".into(),
            size_bytes: 14,
            created_at: "2026-01-01T00:00:00Z".into(),
            expires_at: None,
            downloadable: false,
            scope_id: None,
            logical_path: None,
            harvest_key: None,
            artifact_idempotency_scope: None,
            deleted: false,
        })
        .await
        .unwrap();
    let managed = crate::ManagedHost::new(host.clone());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: public_id,
        mount_path: "/mnt/input.bin".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    let error = managed
        .install_test_session_init("t-corrupt-file", init)
        .await;

    assert!(
        error
            .unwrap_err()
            .message
            .contains("content digest mismatch"),
        "corrupt content must fail before Agent execution"
    );
}

#[tokio::test]
async fn file_activation_enforces_workspace_ownership_without_iam_policy_logic() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let file_id = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            "workspace-a",
            "input.txt".into(),
            "text/plain".into(),
            b"workspace-a",
        )
        .await
        .unwrap()
        .id;
    let managed = crate::ManagedHost::new(host);
    let mut init = bare_session("a", "workspace-b");
    init.resources = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/mnt/input.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    let error = managed
        .install_test_session_init("t-cross-workspace", init)
        .await;

    assert!(
        error
            .unwrap_err()
            .message
            .contains("not found in this workspace"),
        "resource integrity rejects a foreign Workspace id without parsing IAM policy"
    );
}
