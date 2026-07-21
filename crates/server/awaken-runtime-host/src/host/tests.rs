use super::*;
use crate::config::block_text;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_protocol_managed::resource_plane as awaken_resource_contract;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
use std::sync::atomic::AtomicUsize;

/// Authoring shorthand used only by tests. Production crosses the runtime port
/// exclusively as `EffectiveSessionInputs`.
#[derive(Clone)]
struct TestInput {
    kind: String,
    id: String,
    mount_path: String,
    access: awaken_resource_contract::ResourceAccess,
    instructions: Option<String>,
    git_ref: Option<String>,
}

use awaken_resource_contract::ResourceAccess;

struct TestResourceConfigSource;

impl awaken_resource_contract::ResourceConfigSource for TestResourceConfigSource {
    fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<
        awaken_resource_contract::ResolvedMemoryStoreConfig,
        awaken_resource_contract::ResourceCatalogError,
    > {
        Ok(awaken_resource_contract::ResolvedMemoryStoreConfig {
            definition: awaken_resource_contract::MemoryStoreDefinition {
                id: id.into(),
                workspace_id: workspace_id.into(),
                name: id.into(),
                description: String::new(),
                metadata: Default::default(),
                state: awaken_resource_contract::ResourceState::Active,
                current_config_version: awaken_resource_contract::ConfigVersion::INITIAL,
            },
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: id.into(),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                recall_policy: Default::default(),
                extraction_policy: Default::default(),
                retention_policy: Default::default(),
            },
        })
    }

    fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<
        awaken_resource_contract::ResolvedRepositoryConfig,
        awaken_resource_contract::ResourceCatalogError,
    > {
        Ok(awaken_resource_contract::ResolvedRepositoryConfig {
            definition: awaken_resource_contract::RepositoryDefinition {
                id: id.into(),
                workspace_id: workspace_id.into(),
                name: id.into(),
                description: String::new(),
                metadata: Default::default(),
                state: awaken_resource_contract::ResourceState::Active,
                current_config_version: awaken_resource_contract::ConfigVersion::INITIAL,
            },
            config: awaken_resource_contract::RepositoryConfigVersion {
                repository_id: id.into(),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                remote_url: "https://unused.example/repo.git".into(),
                credential_binding: None,
                initial_branch: None,
                clone_policy: Default::default(),
            },
        })
    }
}

fn managed_with_resource_source(host: Arc<SharedHost>) -> crate::ManagedHost {
    crate::ManagedHost::new(host).with_resource_configs(Arc::new(TestResourceConfigSource))
}

fn effective_resources(
    resources: Vec<TestInput>,
) -> awaken_protocol_managed::EffectiveSessionInputs {
    use awaken_protocol_managed::{ResolvedInput, ResolvedInputSource};
    use awaken_resource_contract::{
        BindingId, FileId, MemoryStoreId, RepositoryId, ResourceAccess,
    };

    awaken_protocol_managed::EffectiveSessionInputs {
        inputs: resources
            .into_iter()
            .enumerate()
            .map(|(index, resource)| {
                let source = match resource.kind.as_str() {
                    "file" => ResolvedInputSource::File {
                        file_id: FileId::from(resource.id.clone()),
                    },
                    "memory_store" => ResolvedInputSource::MemoryStore {
                        memory_store_id: MemoryStoreId::from(resource.id.clone()),
                        config: awaken_resource_contract::MemoryStoreConfigVersion {
                            memory_store_id: resource.id.clone(),
                            version: awaken_resource_contract::ConfigVersion::INITIAL,
                            recall_policy: Default::default(),
                            extraction_policy: Default::default(),
                            retention_policy: Default::default(),
                        },
                    },
                    "github_repository" => ResolvedInputSource::Repository {
                        repository_id: RepositoryId::new(format!("test-repo-{index}")),
                        config: awaken_resource_contract::RepositoryConfigVersion {
                            repository_id: format!("test-repo-{index}"),
                            version: awaken_resource_contract::ConfigVersion::INITIAL,
                            remote_url: resource.id,
                            credential_binding: None,
                            initial_branch: resource.git_ref,
                            clone_policy: Default::default(),
                        },
                    },
                    kind => panic!("unsupported test input kind {kind}"),
                };
                ResolvedInput {
                    binding_id: BindingId::new(format!("test-input-{index}")),
                    source,
                    mount_path: resource.mount_path,
                    access: match (resource.kind.as_str(), resource.access) {
                        ("file", _) | (_, ResourceAccess::ReadOnly) => ResourceAccess::ReadOnly,
                        (_, ResourceAccess::ReadWrite) => ResourceAccess::ReadWrite,
                    },
                    instructions: resource.instructions,
                }
            })
            .collect(),
    }
}

fn effective_repository(
    id: &str,
    url: &str,
    mount_path: &str,
    credential_binding: Option<String>,
) -> awaken_protocol_managed::EffectiveSessionInputs {
    awaken_protocol_managed::EffectiveSessionInputs {
        inputs: vec![awaken_protocol_managed::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from("test-repository"),
            source: awaken_protocol_managed::ResolvedInputSource::Repository {
                repository_id: awaken_resource_contract::RepositoryId::from(id),
                config: awaken_resource_contract::RepositoryConfigVersion {
                    repository_id: id.into(),
                    version: awaken_resource_contract::ConfigVersion::INITIAL,
                    remote_url: url.into(),
                    credential_binding,
                    initial_branch: None,
                    clone_policy: Default::default(),
                },
            },
            mount_path: mount_path.into(),
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
        }],
    }
}

fn carried_mount_bytes(mount: &awaken_provisioning_contract::MountRequirement) -> Vec<u8> {
    let awaken_provisioning_contract::MountSource::InlineBytes { contents, .. } = &mount.source
    else {
        panic!("expected a carried resource source")
    };
    contents.clone()
}

fn memory_mount_store_id(mount: &awaken_provisioning_contract::MountRequirement) -> &str {
    let awaken_provisioning_contract::MountSource::MemoryStore { store_id } = &mount.source else {
        panic!("expected a governed memory-store source")
    };
    store_id
}

/// Test composition adapter for runtime-host's dependency-inverted MemoryMounter
/// port. Production installs `awaken-sandbox-memoryd` from awaken-server.
struct TestMemoryMounter {
    fs: Arc<dyn awaken_memory_store::MemoryFs>,
}

struct TestMemoryMount;

#[async_trait::async_trait]
impl awaken_provisioning_contract::MemoryMount for TestMemoryMount {
    fn realization(&self) -> awaken_provisioning_contract::Realization {
        awaken_provisioning_contract::Realization::Copy
    }

    async fn teardown(self: Box<Self>) {}
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::MemoryMounter for TestMemoryMounter {
    async fn mount(
        &self,
        store_id: &str,
        host_path: &std::path::Path,
        _access: awaken_provisioning_contract::MountAccess,
    ) -> Result<
        Box<dyn awaken_provisioning_contract::MemoryMount>,
        awaken_provisioning_contract::SandboxError,
    > {
        std::fs::create_dir_all(host_path)
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))?;
        for entry in
            self.fs.list(store_id, "/").await.map_err(|error| {
                awaken_provisioning_contract::SandboxError::new(error.to_string())
            })?
        {
            let Some(memory) =
                self.fs
                    .get_by_path(store_id, &entry.path)
                    .await
                    .map_err(|error| {
                        awaken_provisioning_contract::SandboxError::new(error.to_string())
                    })?
            else {
                continue;
            };
            let path = host_path.join(memory.path.trim_start_matches('/'));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    awaken_provisioning_contract::SandboxError::new(error.to_string())
                })?;
            }
            std::fs::write(path, memory.content.unwrap_or_default()).map_err(|error| {
                awaken_provisioning_contract::SandboxError::new(error.to_string())
            })?;
        }
        Ok(Box::new(TestMemoryMount))
    }
}

fn install_test_memory_mounter(host: &SharedHost) {
    host.install_memory_mounter(Arc::new(TestMemoryMounter {
        fs: host.memory_fs(),
    }));
}

/// A model that blocks on its second inference (the first revision round) until
/// a gate is released, so a concurrent `interrupt` can land while the outcome
/// loop is mid-run. Its reply never contains the rubric, so the guard steers.
struct GatedModel {
    gate: Arc<tokio::sync::Notify>,
    reached: Arc<tokio::sync::Notify>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for GatedModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            self.reached.notify_one();
            self.gate.notified().await;
        }
        Ok(ChatResponse {
            output: AssistantOutput::text("a rough draft"),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_cancels_the_run_and_reports_interrupted() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: gate.clone(),
        reached: reached.clone(),
        calls: AtomicUsize::new(0),
    });
    let host = Arc::new(SharedHost::new(model, "scripted"));

    // Drive an outcome whose rubric is never met, so it would loop; the model
    // blocks it mid second round.
    let driver = host.clone();
    let task = tokio::spawn(async move { driver.define_outcome("t1", "finish", "FINAL", 5).await });

    // Once the loop is blocked mid-run, interrupt it, then release the gate.
    reached.notified().await;
    host.interrupt("t1").await.expect("interrupt");
    gate.notify_one();

    let report = task.await.expect("join").expect("define_outcome");
    // Round 1 graded needs_revision; the interrupt ended the run before the
    // second round could conclude, so the outcome reports interrupted.
    assert_eq!(report.iterations[0].result, "needs_revision");
    assert_eq!(
        report.iterations.last().expect("a round").result,
        "interrupted"
    );
}

#[tokio::test]
async fn interrupt_is_a_noop_when_nothing_runs() {
    let host = SharedHost::new(
        Arc::new(GatedModel {
            gate: Arc::new(tokio::sync::Notify::new()),
            reached: Arc::new(tokio::sync::Notify::new()),
            calls: AtomicUsize::new(0),
        }),
        "scripted",
    );
    // No run in flight → interrupt succeeds and does nothing.
    host.interrupt("idle-thread")
        .await
        .expect("interrupt is a no-op");
}

/// A model that blocks on its first inference until released, so a concurrent
/// `interrupt` lands while a plain `run` turn is mid-flight.
struct BlockOnceModel {
    reached: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl LlmExecutor for BlockOnceModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.reached.notify_one();
        self.gate.notified().await;
        Ok(ChatResponse {
            output: AssistantOutput::text("too late"),
            usage: None,
            stop_reason: None,
        })
    }
}

/// The real-turn interrupt the conformance matrix flagged as unasserted: a plain
/// `run` (a managed session's normal turn), interrupted while its inference is in
/// flight, ends `Cancelled` promptly instead of running to completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_ends_an_in_flight_run_as_cancelled() {
    let reached = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let host = Arc::new(SharedHost::new(
        Arc::new(BlockOnceModel {
            reached: reached.clone(),
            gate: gate.clone(),
        }),
        "scripted",
    ));

    let driver = host.clone();
    let task = tokio::spawn(async move { driver.run(None, "t-int", user("go")).await });

    // The turn is blocked mid-inference; interrupt it, then release the gate.
    reached.notified().await;
    host.interrupt("t-int").await.expect("interrupt");
    gate.notify_one();

    let result = task.await.expect("join").expect("run");
    assert!(
        matches!(result.state, RunState::Ended(EndCause::Cancelled)),
        "an interrupted in-flight turn ends Cancelled, not run to completion: {:?}",
        result.state
    );
}

/// The main assistant answers plainly; the memory extractor (identified by its
/// system instructions) saves one memory then reports done.
struct MemoryHostModel;

#[async_trait::async_trait]
impl LlmExecutor for MemoryHostModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let is_extractor = request.messages.iter().any(|m| {
            m.role == Role::System
                && m.content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("memory extraction Agent"),
                    _ => false,
                })
        });
        let output = if is_extractor {
            if request.messages.iter().any(|m| m.role == Role::Tool) {
                AssistantOutput::text("saved 1 memory")
            } else {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "w".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({
                        "name": "user prefs",
                        "content": "user likes rust",
                    }),
                }])
            }
        } else {
            AssistantOutput::text("ok")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// The compactor (identified by its instructions) replies with a fixed summary;
/// the main assistant reports whether it saw a delivered summary in its system
/// messages, proving the summary reached the next turn's model input.
struct CompactHostModel;

#[async_trait::async_trait]
impl LlmExecutor for CompactHostModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let system_text: String = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let reply = if system_text.contains("conversation-compaction Agent") {
            "COMPACTED".to_string()
        } else if system_text.contains("Summary of earlier conversation") {
            "seen-summary".to_string()
        } else {
            "no-summary".to_string()
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn compaction_summary_reaches_the_same_long_turn() {
    let host = SharedHost::new(Arc::new(CompactHostModel), "stub").with_compaction(1, 1);
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, "hello")];

    // Turn 1: only the single user message → below threshold, no summary injected.
    let r1 = host.run(None, "t-c", user("u1")).await.expect("turn 1");
    assert!(matches!(r1.state, RunState::Ended(_)));
    let reply1 = r1
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(reply1, "no-summary", "short turn is not compacted");

    // Turn 2: the conversation now exceeds the threshold, so the compact plugin's
    // BeforeInference hook summarizes the older slice inline and the model sees it.
    let r2 = host.run(None, "t-c", user("u2")).await.expect("turn 2");
    let reply2 = r2
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(reply2, "seen-summary");
}

/// The extractor saves "the user prefers tea"; the main agent answers "tea"
/// only when that memory is present in its system context (recalled).
struct MemLoopModel;

#[async_trait::async_trait]
impl LlmExecutor for MemLoopModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let system_text: String = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if system_text.contains("memory extraction Agent") {
            let already = request.messages.iter().any(|m| {
                m.role == Role::Tool
                    && m.content.iter().any(|b| match b {
                        ContentBlock::ToolResult { content, .. } => {
                            block_text(content).contains("saved memory")
                        }
                        _ => false,
                    })
            });
            let output = if already {
                AssistantOutput::text("done")
            } else {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "w".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({
                        "name": "beverage-preference",
                        "content": "the user prefers tea",
                    }),
                }])
            };
            return Ok(ChatResponse {
                output,
                usage: None,
                stop_reason: None,
            });
        }
        // Main agent: answer from recalled memory when present.
        let reply = if system_text.contains("the user prefers tea") {
            "tea"
        } else {
            "ok"
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn memory_written_in_one_thread_is_recalled_and_used_in_another() {
    let stamp = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
    let mem_dir = std::env::temp_dir().join(format!("awaken-loop-mem-{stamp}"));
    let host = SharedHost::new(Arc::new(MemLoopModel), "stub").with_memory(&mem_dir);
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    // Thread 1: the user states a preference; extraction saves it.
    host.run(None, "thread-1", user("I really enjoy tea in the morning"))
        .await
        .expect("thread 1 turn");
    assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
    assert!(
        mem_dir.join("beverage-preference.md").exists(),
        "the preference should be saved"
    );

    // Thread 2 (a fresh conversation): the saved memory is recalled into context
    // and the agent uses it to answer.
    let r = host
        .run(None, "thread-2", user("What beverage do I prefer?"))
        .await
        .expect("thread 2 turn");
    let reply = r
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(
        reply, "tea",
        "the fresh thread should recall and use the saved memory"
    );
}

/// Managed Memory never falls back to the standalone host directory: the frozen
/// Session input chooses one store, and that same handle serves extraction + recall.
#[tokio::test]
async fn managed_memory_is_per_store_and_an_unbound_session_cannot_see_host_memory() {
    use awaken_protocol_managed::{ResolvedInputSource, SessionInit, SessionRuntime};

    let stamp = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
    let global = std::env::temp_dir().join(format!("awaken-managed-global-{stamp}"));
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(global.join("must-not-leak.md"), "the user prefers tea").unwrap();

    let host = Arc::new(SharedHost::new(Arc::new(MemLoopModel), "stub").with_memory(&global));
    install_test_memory_mounter(&host);
    let store_a = host.create_memory_store().await;
    let store_b = host.create_memory_store().await;
    let managed = managed_with_resource_source(host.clone());
    let init = |store: Option<&str>, extraction_enabled: bool| {
        let mut init = SessionInit {
            workspace_id: host.local_workspace().into(),
            agent_id: "agent".into(),
            mcp_servers: Vec::new(),
            resources: effective_resources(
                store
                    .map(|id| TestInput {
                        kind: "memory_store".into(),
                        id: id.into(),
                        mount_path: "/memory".into(),
                        // Workdir cannot OS-enforce read-only mounts, so this integration
                        // path uses a writable mount with extraction disabled. The
                        // handle-level read-only invariant is covered separately.
                        access: ResourceAccess::ReadWrite,
                        instructions: None,
                        git_ref: None,
                    })
                    .into_iter()
                    .collect(),
            ),
            model: None,
            runtime: None,
            deny_egress: false,
            sandbox: None,
        };
        if let Some(input) = init.resources.inputs.first_mut()
            && let ResolvedInputSource::MemoryStore { config, .. } = &mut input.source
        {
            config.extraction_policy.enabled = extraction_enabled;
        }
        init
    };

    managed
        .prepare_session("managed-write-a", init(Some(&store_a), true))
        .await
        .unwrap();
    managed
        .run(
            "agent",
            "managed-write-a",
            vec![ContentBlock::text("I enjoy tea")],
        )
        .await
        .unwrap();
    assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
    assert!(
        host.memory_stores
            .fs()
            .get_by_path(&store_a, "/beverage-preference.md")
            .await
            .unwrap()
            .is_some(),
        "extraction writes the bound platform store"
    );
    assert!(
        host.memory_stores
            .fs()
            .list(&store_b, "/")
            .await
            .unwrap()
            .is_empty(),
        "a different store remains untouched"
    );

    let reply = |outcome: &awaken_protocol_managed::StepOutcome| {
        outcome
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(|message| block_text(&message.content))
            .unwrap_or_default()
    };
    managed
        .prepare_session("managed-read-a", init(Some(&store_a), false))
        .await
        .unwrap();
    let same = managed
        .run(
            "agent",
            "managed-read-a",
            vec![ContentBlock::text("What do I prefer?")],
        )
        .await
        .unwrap();
    assert_eq!(reply(&same), "tea", "recall reads the same bound store");

    managed
        .prepare_session("managed-read-b", init(Some(&store_b), false))
        .await
        .unwrap();
    let other = managed
        .run(
            "agent",
            "managed-read-b",
            vec![ContentBlock::text("What do I prefer?")],
        )
        .await
        .unwrap();
    assert_eq!(reply(&other), "ok", "store B cannot recall store A");

    managed
        .prepare_session("managed-unbound", init(None, false))
        .await
        .unwrap();
    let unbound = managed
        .run(
            "agent",
            "managed-unbound",
            vec![ContentBlock::text("What do I prefer?")],
        )
        .await
        .unwrap();
    assert_eq!(
        reply(&unbound),
        "ok",
        "Managed explicitly records no binding and never sees host-global memory"
    );
}

#[tokio::test]
async fn pinned_memory_policy_can_disable_recall_and_extraction() {
    use awaken_protocol_managed::{ResolvedInputSource, SessionRuntime};

    let stamp = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
    let global = std::env::temp_dir().join(format!("awaken-managed-policy-{stamp}"));
    let host = Arc::new(SharedHost::new(Arc::new(MemLoopModel), "stub").with_memory(global));
    install_test_memory_mounter(&host);
    let store = host.create_memory_store().await;
    host.memory_stores
        .fs()
        .create(&store, "/existing.md", "the user prefers tea")
        .await
        .unwrap();
    let managed = managed_with_resource_source(host.clone());
    let mut init = bare_session("agent", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store.clone(),
        mount_path: "/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        git_ref: None,
    }]);
    let ResolvedInputSource::MemoryStore { config, .. } = &mut init.resources.inputs[0].source
    else {
        unreachable!()
    };
    config.recall_policy.enabled = false;
    config.extraction_policy.enabled = false;

    managed
        .prepare_session("managed-policy", init)
        .await
        .unwrap();
    let outcome = managed
        .run(
            "agent",
            "managed-policy",
            vec![ContentBlock::text("What do I prefer?")],
        )
        .await
        .unwrap();
    let reply = outcome
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| block_text(&message.content))
        .unwrap_or_default();
    assert_eq!(reply, "ok", "disabled recall does not inject store content");
    assert!(host.drain_memory(std::time::Duration::from_secs(1)).await);
    assert!(
        host.memory_stores
            .fs()
            .get_by_path(&store, "/beverage-preference.md")
            .await
            .unwrap()
            .is_none(),
        "disabled extraction does not mutate the store"
    );
}

/// The main agent awaits on a `write` (Ask-gated) then finishes on resume; the
/// extractor saves a memory. Proves resume-ended turns trigger the aux agents.
struct ResumeMemModel;

#[async_trait::async_trait]
impl LlmExecutor for ResumeMemModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let saw_tool = request.messages.iter().any(|m| m.role == Role::Tool);
        // The extractor's own write_memory succeeded (its result text), distinct
        // from the main turn's `write` result that is also in its seeded context.
        let saved_memory = request.messages.iter().any(|m| {
            m.role == Role::Tool
                && m.content.iter().any(|b| match b {
                    ContentBlock::ToolResult { content, .. } => {
                        block_text(content).contains("saved memory")
                    }
                    _ => false,
                })
        });
        let is_extractor = request.messages.iter().any(|m| {
            m.role == Role::System
                && m.content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("memory extraction Agent"),
                    _ => false,
                })
        });
        let output = if is_extractor {
            if saved_memory {
                AssistantOutput::text("extracted")
            } else {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "mw".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({ "name": "resumed", "content": "after-resume" }),
                }])
            }
        } else if saw_tool {
            AssistantOutput::text("done")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w1".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({ "path": "note.txt", "content": "x" }),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn resume_ended_turn_triggers_memory_extraction() {
    let stamp = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
    let mem_dir = std::env::temp_dir().join(format!("awaken-resume-mem-{stamp}"));
    let host = SharedHost::new(Arc::new(ResumeMemModel), "stub").with_memory(&mem_dir);

    // Turn 1 awaits on the Ask-gated `write`.
    let r1 = host
        .run(
            None,
            "t-res",
            vec![Message::text(MessageId("u1".into()), Role::User, "hi")],
        )
        .await
        .expect("turn 1");
    assert!(
        matches!(r1.state, RunState::Awaiting),
        "turn should await on write"
    );
    let pending = r1.pending.expect("a pending tool");

    // Resume approves the write; the turn now ends and extraction fires.
    let r2 = host
        .resume(
            "t-res",
            &pending.tool_use_id,
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
        )
        .await
        .expect("resume");
    assert!(
        matches!(r2.state, RunState::Ended(_)),
        "resume should end the turn"
    );

    assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
    let saved = std::fs::read_to_string(mem_dir.join("resumed.md")).expect("memory file");
    assert_eq!(saved, "after-resume");
}

/// The extractor writes a `seen.md` whose content is the non-prompt user texts
/// it was seeded with, so a test can check which messages each extraction saw.
struct CursorModel;

#[async_trait::async_trait]
impl LlmExecutor for CursorModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let is_extractor = request.messages.iter().any(|m| {
            m.role == Role::System
                && m.content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("memory extraction Agent"),
                    _ => false,
                })
        });
        if !is_extractor {
            return Ok(ChatResponse {
                output: AssistantOutput::text("ok"),
                usage: None,
                stop_reason: None,
            });
        }
        if request.messages.iter().any(|m| m.role == Role::Tool) {
            return Ok(ChatResponse {
                output: AssistantOutput::text("extracted"),
                usage: None,
                stop_reason: None,
            });
        }
        // Join the user texts it was seeded with, excluding the extraction prompt.
        let seen: Vec<String> = request
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| block_text(&m.content))
            .filter(|t| !t.contains("Extract durable memories"))
            .collect();
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w".into(),
                tool_id: "write_memory".into(),
                arguments: serde_json::json!({ "name": "seen", "content": seen.join(",") }),
            }]),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn extraction_cursor_only_processes_new_messages() {
    let stamp = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
    let mem_dir = std::env::temp_dir().join(format!("awaken-cursor-mem-{stamp}"));
    let host = SharedHost::new(Arc::new(CursorModel), "stub").with_memory(&mem_dir);
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    host.run(None, "t-cur", user("alpha"))
        .await
        .expect("turn 1");
    assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
    host.run(None, "t-cur", user("beta")).await.expect("turn 2");
    assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);

    // The second extraction saw only "beta" — turn 1's "alpha" was past the cursor.
    let seen = std::fs::read_to_string(mem_dir.join("seen.md")).expect("seen file");
    assert_eq!(
        seen, "beta",
        "cursor should exclude already-extracted messages"
    );
}

#[tokio::test]
async fn turn_end_fires_background_memory_extraction() {
    let stamp = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
    let mem_dir = std::env::temp_dir().join(format!("awaken-host-mem-{stamp}"));
    let host = SharedHost::new(Arc::new(MemoryHostModel), "stub").with_memory(&mem_dir);

    let input = vec![Message::text(
        MessageId("u1".into()),
        Role::User,
        "I really like rust",
    )];
    let result = host.run(None, "t-mem", input).await.expect("run turn");
    assert!(
        matches!(result.state, RunState::Ended(_)),
        "turn should end"
    );

    let drained = host.drain_memory(std::time::Duration::from_secs(10)).await;
    assert!(drained, "memory extraction should drain");

    let saved =
        std::fs::read_to_string(mem_dir.join("user-prefs.md")).expect("memory file written");
    assert_eq!(saved, "user likes rust");
}

/// A trivial model for resource-lifecycle turns.
struct OkModel;
#[async_trait::async_trait]
impl LlmExecutor for OkModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("ok"),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Hot-attach is realized, not a bookkeeping edit: attaching a file to a live
/// session stages its mount AND evicts the cached sandbox, so the NEXT turn
/// rebuilds with the file mounted; detaching reverses it.
#[tokio::test]
async fn applying_changed_inputs_rebuilds_the_resource_projection_and_cached_sandbox() {
    use awaken_protocol_managed::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = managed_with_resource_source(host.clone());
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    // A blob to mount, and a first turn that builds + caches the thread's sandbox.
    let file_id = host
        .file_store()
        .put(b"hello-attached")
        .await
        .expect("put blob");
    host.grant_file(host.local_workspace(), &file_id);
    host.run(None, "t-attach", user("hi"))
        .await
        .expect("first turn");
    assert!(
        host.sessions.lock().await.contains_key("t-attach"),
        "the first turn caches the thread's sandbox ctx"
    );
    let before = host.sandbox_spec("t-attach").mounts.len();

    // Attach a file resource on the LIVE session.
    let res = TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/data.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        git_ref: None,
    };
    let attached = effective_resources(vec![res.clone()]);
    managed
        .apply_session_inputs("t-attach", host.local_workspace(), &attached)
        .await
        .expect("attach");

    // The cached sandbox was evicted (so the next turn rebuilds) ...
    assert!(
        !host.sessions.lock().await.contains_key("t-attach"),
        "attach evicts the cached ctx so the next turn rebuilds with the mount"
    );
    // ... and the spec the next turn will build now carries the mount + its bytes.
    let spec = host.sandbox_spec("t-attach");
    assert_eq!(spec.mounts.len(), before + 1, "one more mount staged");
    let dump = serde_json::to_string(&spec.mounts).expect("mounts serialize");
    assert!(
        dump.contains("data.txt"),
        "mount realized at the resource path: {dump}"
    );
    assert_eq!(
        carried_mount_bytes(spec.mounts.last().unwrap()),
        b"hello-attached"
    );

    // Detach removes exactly that mount again.
    managed
        .apply_session_inputs(
            "t-attach",
            host.local_workspace(),
            &awaken_protocol_managed::EffectiveSessionInputs::default(),
        )
        .await
        .expect("detach");
    let spec = host.sandbox_spec("t-attach");
    assert_eq!(spec.mounts.len(), before, "the mount is dropped on detach");
    assert!(
        !serde_json::to_string(&spec.mounts)
            .unwrap()
            .contains("data.txt")
    );
}

/// The environment's egress policy reaches the sandbox: a `deny_egress` SessionInit
/// stages the thread so its rebuilt sandbox spec denies network egress; an
/// unrestricted one leaves the host network shared.
#[tokio::test]
async fn prepare_session_stages_egress_into_the_sandbox_spec() {
    use awaken_protocol_managed::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = managed_with_resource_source(host.clone());
    let init = |deny: bool| SessionInit {
        workspace_id: host.local_workspace().into(),
        agent_id: "a".into(),
        mcp_servers: Vec::new(),
        resources: Default::default(),
        model: None,
        runtime: None,
        deny_egress: deny,
        sandbox: None,
    };

    // Egress denial rides the Workdir spec's opaque `extra` (a bwrap convenience,
    // not admission-gated network isolation this tier cannot enforce).
    let denies = |spec: awaken_provisioning_contract::SandboxSpec| {
        spec.extra
            .as_ref()
            .and_then(|v| v.get("deny_egress"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };

    managed.prepare_session("t-deny", init(true)).await.unwrap();
    assert!(
        denies(host.sandbox_spec("t-deny")),
        "a deny_egress session stages into the sandbox spec"
    );

    managed
        .prepare_session("t-open", init(false))
        .await
        .unwrap();
    assert!(
        !denies(host.sandbox_spec("t-open")),
        "an unrestricted session keeps the host network"
    );
}

/// The environment's `config.sandbox` overlay reaches the sandbox spec: a session whose
/// `SessionInit.sandbox` sets isolation/network/limits stages the thread so its rebuilt
/// spec reflects them (superseding the hardcoded Workdir/Unrestricted defaults). This is
/// the S4 chain end: env config → SessionInit → prepare_session → sandbox_spec.
#[tokio::test]
async fn prepare_session_overlays_the_environment_sandbox_onto_the_spec() {
    use awaken_protocol_managed::{SessionInit, SessionRuntime};
    use awaken_provisioning_contract::{IsolationClass, NetworkPolicy};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone());

    // The session carries the raw `config.sandbox` blob (the host parses it).
    let init = SessionInit {
        workspace_id: "ws".into(),
        agent_id: "a".into(),
        mcp_servers: Vec::new(),
        resources: Default::default(),
        model: None,
        runtime: None,
        deny_egress: false,
        sandbox: Some(serde_json::json!({
            "isolation": "namespace",
            "network": { "mode": "allowlist", "hosts": ["api.github.com"] },
            "limits": { "cpu_millis": 2000, "memory_bytes": 4294967296u64 }
        })),
    };
    managed.prepare_session("t-sb", init).await.unwrap();

    let spec = host.sandbox_spec("t-sb");
    assert_eq!(
        spec.isolation,
        IsolationClass::Namespace,
        "env isolation overlaid"
    );
    assert_eq!(
        spec.network,
        NetworkPolicy::Allowlist {
            hosts: vec!["api.github.com".into()]
        },
        "env allowlist supersedes the default unrestricted network"
    );
    assert_eq!(spec.limits.cpu_millis, Some(2000));
    assert_eq!(spec.limits.memory_bytes, Some(4_294_967_296));

    // A session with no override keeps the host default (Workdir, no limits).
    let bare = SessionInit {
        workspace_id: "ws".into(),
        agent_id: "a".into(),
        mcp_servers: Vec::new(),
        resources: Default::default(),
        model: None,
        runtime: None,
        deny_egress: false,
        sandbox: None,
    };
    managed.prepare_session("t-bare", bare).await.unwrap();
    assert_eq!(
        host.sandbox_spec("t-bare").isolation,
        IsolationClass::Workdir
    );
    assert!(!host.sandbox_spec("t-bare").limits.is_set());
}

/// Runtime stages the effective resources supplied by the Session control plane. It
/// does not need the Agent binding repository, which keeps remote workers stateless.
#[tokio::test]
async fn prepare_session_mounts_an_effective_memory_resource() {
    use awaken_protocol_managed::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));

    // Seed a memory store with known bytes. The control plane has already resolved
    // this resource into the SessionInit passed across the runtime boundary.
    let store_id = host.create_memory_store().await;
    host.memory_stores
        .fs()
        .create(&store_id, "/facts.md", "the secret code is BANANA-42")
        .await
        .expect("seed memory");
    let managed = managed_with_resource_source(host.clone());

    let bare = |agent: &str| SessionInit {
        workspace_id: host.local_workspace().into(),
        agent_id: agent.into(),
        mcp_servers: Vec::new(),
        resources: effective_resources(
            (agent == "a")
                .then(|| TestInput {
                    kind: "memory_store".into(),
                    id: store_id.clone(),
                    mount_path: "/mnt/memory".into(),
                    access: ResourceAccess::ReadWrite,
                    instructions: None,
                    git_ref: None,
                })
                .into_iter()
                .collect(),
        ),
        model: None,
        runtime: None,
        deny_egress: false,
        sandbox: None,
    };

    // An effective Session input mounts without a ResourceStore on the host.
    managed.prepare_session("t-bound", bare("a")).await.unwrap();
    let dump = serde_json::to_string(&host.sandbox_spec("t-bound").mounts).unwrap();
    assert!(
        dump.contains("mnt/memory"),
        "the bound memory store is mounted at its path: {dump}"
    );
    assert_eq!(
        memory_mount_store_id(&host.sandbox_spec("t-bound").mounts[0]),
        store_id
    );
    assert_eq!(host.thread_memory_mounts("t-bound").len(), 1);

    let mut read_only = bare("a");
    read_only.resources.inputs[0].access = awaken_resource_contract::ResourceAccess::ReadOnly;
    managed
        .prepare_session("t-read-only", read_only)
        .await
        .unwrap();
    assert_eq!(
        host.sandbox_spec("t-read-only").mounts[0].access,
        awaken_provisioning_contract::MountAccess::ReadOnly
    );
    assert!(
        host.thread_memory_mounts("t-read-only").is_empty(),
        "read-only Memory inputs must never enter the write-back/harvest set"
    );

    // An empty effective input set mounts nothing.
    managed
        .prepare_session("t-unbound", bare("no-bindings"))
        .await
        .unwrap();
    let empty = serde_json::to_string(&host.sandbox_spec("t-unbound").mounts).unwrap();
    assert!(
        !empty.contains("BANANA-42"),
        "an unbound agent mounts nothing extra: {empty}"
    );
}

#[tokio::test]
async fn activation_applies_current_resource_state_as_a_deny_only_overlay() {
    use awaken_protocol_managed::SessionRuntime;
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, ResourceCatalog,
        ResourceState,
    };

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = host.create_memory_store().await;
    let catalog = Arc::new(awaken_config_resolver::InMemoryResourceCatalog::new());
    catalog
        .create_memory_store(
            MemoryStoreDefinition {
                id: store_id.clone(),
                workspace_id: host.local_workspace().into(),
                name: "memory".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
            },
            MemoryStoreConfigVersion {
                memory_store_id: store_id.clone(),
                version: ConfigVersion::INITIAL,
                recall_policy: Default::default(),
                extraction_policy: Default::default(),
                retention_policy: Default::default(),
            },
        )
        .unwrap();
    let manifest = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        git_ref: None,
    }]);
    catalog
        .set_memory_state(host.local_workspace(), &store_id, ResourceState::Suspended)
        .unwrap();
    let managed = crate::ManagedHost::new(host.clone()).with_resource_configs(catalog);
    let mut init = bare_session("a", host.local_workspace());
    init.resources = manifest;

    let error = managed
        .prepare_session("t-suspended", init)
        .await
        .unwrap_err();

    assert!(error.message.contains("not active"));
    assert!(host.sandbox_spec("t-suspended").mounts.is_empty());
}

#[tokio::test]
async fn memory_activation_enforces_catalog_workspace_without_iam_policy_logic() {
    use awaken_protocol_managed::SessionRuntime;
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, ResourceCatalog,
        ResourceState,
    };

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = host.create_memory_store().await;
    let catalog = Arc::new(awaken_config_resolver::InMemoryResourceCatalog::new());
    catalog
        .create_memory_store(
            MemoryStoreDefinition {
                id: store_id.clone(),
                workspace_id: "workspace-a".into(),
                name: "private-memory".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
            },
            MemoryStoreConfigVersion {
                memory_store_id: store_id.clone(),
                version: ConfigVersion::INITIAL,
                recall_policy: Default::default(),
                extraction_policy: Default::default(),
                retention_policy: Default::default(),
            },
        )
        .unwrap();
    let managed = crate::ManagedHost::new(host.clone()).with_resource_configs(catalog);
    let mut init = bare_session("agent", "workspace-b");
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store_id,
        mount_path: "/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        git_ref: None,
    }]);

    let error = managed
        .prepare_session("wrong-workspace", init)
        .await
        .unwrap_err();
    assert!(error.message.contains("not found"));
    assert!(host.sandbox_spec("wrong-workspace").mounts.is_empty());
    assert!(host.memory_for_thread("wrong-workspace").is_none());
}

/// The same effective input contract realizes File and Repository resources without
/// exposing their authoring repository to Runtime.
#[tokio::test]
async fn prepare_session_mounts_effective_file_and_stages_effective_repo() {
    use awaken_protocol_managed::{SessionInit, SessionRuntime};
    use awaken_provisioning_contract::{MountAccess, MountSource};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));

    // Seed a file blob and pass the already-resolved File and Repository inputs.
    let binary = vec![0, 0xff, b'R', 0x80, b'\n'];
    let file_id = host.file_store().put(&binary).await.expect("put blob");
    host.grant_file(host.local_workspace(), &file_id);
    let managed = managed_with_resource_source(host.clone());

    managed
        .prepare_session(
            "t-multi",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                mcp_servers: Vec::new(),
                resources: effective_resources(vec![
                    TestInput {
                        kind: "file".into(),
                        id: file_id.clone(),
                        mount_path: "/mnt/files/notes.txt".into(),
                        access: ResourceAccess::ReadOnly,
                        instructions: None,
                        git_ref: None,
                    },
                    TestInput {
                        kind: "github_repository".into(),
                        id: "https://github.com/awaken/example.git".into(),
                        mount_path: "/mnt/repo".into(),
                        access: ResourceAccess::ReadOnly,
                        instructions: None,
                        git_ref: None,
                    },
                ]),
                model: None,
                runtime: None,
                deny_egress: false,
                sandbox: None,
            },
        )
        .await
        .unwrap();

    // The exact bytes and their content identity cross the neutral mount contract;
    // no UTF-8 conversion can corrupt binary input.
    let spec = host.sandbox_spec("t-multi");
    let mount = &spec.mounts[0];
    assert_eq!(mount.mount_path, ".mnt/mnt/files/notes.txt");
    assert_eq!(mount.access, MountAccess::ReadOnly);
    let MountSource::InlineBytes {
        contents,
        content_hash,
    } = &mount.source
    else {
        panic!("effective File input must use the binary-safe carried source")
    };
    assert_eq!(contents, &binary);
    assert_eq!(content_hash.as_deref(), Some(file_id.as_str()));

    // The repo is staged for a host-side clone (not a byte mount).
    let repos = host.thread_repos("t-multi");
    assert_eq!(repos.len(), 1, "the bound repo is staged for cloning");
    assert_eq!(repos[0].url, "https://github.com/awaken/example.git");
}

#[tokio::test]
async fn file_activation_rejects_bytes_that_do_not_match_the_file_id() {
    use awaken_file_store::{FileStore, FileStoreError};
    use awaken_protocol_managed::SessionRuntime;

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

    let declared_id = awaken_sandbox_local::content_fingerprint(b"declared bytes");
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub");
    raw_host.file_store = Arc::new(CorruptFileStore);
    let host = Arc::new(raw_host);
    host.grant_file(host.local_workspace(), &declared_id);
    let managed = crate::ManagedHost::new(host.clone());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: declared_id,
        mount_path: "/mnt/input.bin".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        git_ref: None,
    }]);

    let error = managed.prepare_session("t-corrupt-file", init).await;

    assert!(
        error.unwrap_err().message.contains("content hash mismatch"),
        "corrupt content must fail before Agent execution"
    );
}

#[tokio::test]
async fn file_activation_enforces_workspace_ownership_without_iam_policy_logic() {
    use awaken_protocol_managed::SessionRuntime;

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let file_id = host.file_store().put(b"workspace-a").await.unwrap();
    host.grant_file("workspace-a", &file_id);
    let managed = crate::ManagedHost::new(host);
    let mut init = bare_session("a", "workspace-b");
    init.resources = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/mnt/input.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        git_ref: None,
    }]);

    let error = managed.prepare_session("t-cross-workspace", init).await;

    assert!(
        error
            .unwrap_err()
            .message
            .contains("not found in this workspace"),
        "resource integrity rejects a foreign Workspace id without parsing IAM policy"
    );
}

/// Managed-Agents model: a `github_repository` session resource clones host-side AND injects
/// a scoped `github:<logical>` MCP server whose token is held host-side — so the agent drives
/// branch/commit/push/PR through MCP tools while the credential never enters the sandbox.
#[tokio::test]
async fn a_github_repository_resource_injects_a_scoped_github_mcp_server() {
    use awaken_protocol_managed::{SessionInit, SessionRuntime};
    use awaken_run_executor_acp::McpCredential;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: host.local_workspace().into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: Some("git".into()),
            env_key: None,
            secret: Some(awaken_agent_contract::RedactedString::from(
                "ghp_secret_token".to_string(),
            )),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    .unwrap();
    let managed = managed_with_resource_source(host.clone()).with_mcp(
        credentials,
        secrets,
        Arc::new(awaken_config_resolver::InMemoryMcpStore::new()),
    );

    managed
        .prepare_session(
            "t-gh",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                mcp_servers: Vec::new(),
                resources: effective_repository(
                    "repo-1",
                    "https://github.com/awaken/example.git",
                    "/workspace/repo",
                    Some(credential.id.0),
                ),
                model: None,
                runtime: None,
                deny_egress: false,
                sandbox: None,
            },
        )
        .await
        .unwrap();

    // The repo is staged for a host-side clone...
    assert_eq!(
        host.thread_repos("t-gh").len(),
        1,
        "repo staged for cloning"
    );

    // ...AND a scoped GitHub MCP server is injected, holding the token host-side.
    let mcp = host.thread_mcp("t-gh");
    let gh = mcp
        .iter()
        .find(|s| s.name == "github:workspace/repo")
        .expect("a github MCP server bridged from the repo resource");
    assert_eq!(gh.url, "https://api.githubcopilot.com/mcp/");
    assert_eq!(
        gh.bearer.as_ref().map(|b| b.expose_secret().to_string()),
        Some("ghp_secret_token".to_string()),
        "the token is held host-side on the prepared MCP server"
    );

    // A SANDBOXED (untrusted) ACP run gets only an α reference — the raw token never enters
    // the sandbox (the whole point of the Managed-Agents server-side-token model).
    match crate::mcp::project_staged_mcp(gh, false, None, "t-gh").credential {
        McpCredential::Reference { reference } => {
            assert_eq!(reference, "session-mcp:github:workspace/repo");
        }
        other => panic!("sandboxed projection must be a secretless reference, got {other:?}"),
    }
    // A trusted (non-sandboxed) run may carry the bearer inline (β) — the split is by isolation.
    match crate::mcp::project_staged_mcp(gh, true, None, "t-gh").credential {
        McpCredential::TrustedInline { secret } => assert_eq!(secret, "ghp_secret_token"),
        other => panic!("trusted projection carries the inline bearer, got {other:?}"),
    }
}

/// Applying a repository manifest with a new credential reference re-keys both
/// the staged clone token and the injected GitHub MCP bearer, host-side.
#[tokio::test]
async fn rotating_a_github_repository_token_re_keys_the_clone_and_mcp_bearer() {
    use awaken_protocol_managed::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: host.local_workspace().into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: Some("git".into()),
            env_key: None,
            secret: Some(awaken_agent_contract::RedactedString::from(
                "ghp_old".to_string(),
            )),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    .unwrap();
    let managed = managed_with_resource_source(host.clone())
        .with_credentials(credentials.clone(), secrets.clone());
    managed
        .prepare_session(
            "t-rot",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                mcp_servers: Vec::new(),
                resources: effective_repository(
                    "repo-1",
                    "https://github.com/awaken/example.git",
                    "/workspace/repo",
                    Some(credential.id.0),
                ),
                model: None,
                runtime: None,
                deny_egress: false,
                sandbox: None,
            },
        )
        .await
        .unwrap();

    let mcp_bearer = |h: &SharedHost| {
        h.thread_mcp("t-rot")
            .into_iter()
            .find(|s| s.name == "github:workspace/repo")
            .and_then(|s| s.bearer.map(|b| b.expose_secret().to_string()))
    };
    let clone_token = |h: &SharedHost| {
        h.thread_repos("t-rot")[0]
            .credential
            .as_ref()
            .map(|t| t.expose_secret().to_string())
    };
    assert_eq!(mcp_bearer(&host).as_deref(), Some("ghp_old"));
    assert_eq!(clone_token(&host).as_deref(), Some("ghp_old"));

    let next_credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: host.local_workspace().into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: Some("git".into()),
            env_key: None,
            secret: Some(awaken_agent_contract::RedactedString::from(
                "ghp_new".to_string(),
            )),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    .unwrap();
    let next = effective_repository(
        "repo-1",
        "https://github.com/awaken/example.git",
        "/workspace/repo",
        Some(next_credential.id.0),
    );

    // The Managed adapter stores the supplied token in the Vault and publishes a
    // new Repository config before invoking this complete-manifest runtime port.
    managed
        .apply_session_inputs("t-rot", host.local_workspace(), &next)
        .await
        .unwrap();

    // Both the injected MCP bearer and the staged clone token are re-keyed to the new token.
    assert_eq!(
        mcp_bearer(&host).as_deref(),
        Some("ghp_new"),
        "MCP bearer rotated"
    );
    assert_eq!(
        clone_token(&host).as_deref(),
        Some("ghp_new"),
        "clone token rotated"
    );
}

// ---------------------------------------------------------------------------
// Resource-plane seam coverage: Runtime consumes one effective Session manifest.
//   G1 consistency  — prompt path/access == realized mount path/access
//   G4 fail-closed  — an effective resource with missing backing aborts preparation
//   G5 distribution — a worker activates carried inputs without an authoring DB
// ---------------------------------------------------------------------------

/// A bare session for `agent` with no wire resources — the common "just run the agent"
/// path where only its bound resources apply.
#[cfg(test)]
fn bare_session(agent: &str, workspace: &str) -> awaken_protocol_managed::SessionInit {
    awaken_protocol_managed::SessionInit {
        workspace_id: workspace.into(),
        agent_id: agent.into(),
        mcp_servers: Vec::new(),
        resources: Default::default(),
        model: None,
        runtime: None,
        deny_egress: false,
        sandbox: None,
    }
}

/// G1 — prompt and mount are derived from the same effective input, including access.
#[tokio::test]
async fn told_equals_mounted_the_prompt_path_and_access_match_the_realized_mount() {
    use awaken_protocol_managed::SessionRuntime;
    use awaken_provisioning_contract::MountAccess;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = host.create_memory_store().await;
    host.memory_stores
        .fs()
        .create(&store_id, "/seed.md", "seed")
        .await
        .expect("seed memory");

    let resource = TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        git_ref: None,
    };
    let managed = managed_with_resource_source(host.clone());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![resource]);
    managed.prepare_session("t-g1", init).await.unwrap();

    let realized = ".mnt/mnt/memory";
    let prompts = host.thread_resource_prompts("t-g1");
    assert!(
        prompts.iter().any(|p| p.contains(realized)),
        "the compiled prompt names the realized path {realized}: {prompts:?}"
    );
    assert!(
        prompts.iter().any(|p| p.contains("read-only")),
        "a read-only binding is described read-only: {prompts:?}"
    );
    let mount = &host.sandbox_spec("t-g1").mounts[0];
    assert_eq!(mount.mount_path, realized);
    assert_eq!(mount.access, MountAccess::ReadOnly);
}

/// Runtime stages exactly the effective resource list it receives and adds no hidden
/// Agent defaults of its own. Replacement is a Session-control-plane decision.
#[tokio::test]
async fn runtime_stages_exactly_the_effective_resource_list() {
    use awaken_protocol_managed::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let s2 = host.create_memory_store().await;
    host.memory_stores
        .fs()
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
        git_ref: None,
    }]);
    managed.prepare_session("t-g3", init).await.unwrap();

    // Exactly one memory mount at that path, and it is the wire store S2.
    let mounts = host.thread_memory_mounts("t-g3");
    let at_path: Vec<_> = mounts
        .iter()
        .filter(|mount| mount.logical == "mnt/memory")
        .collect();
    assert_eq!(
        at_path.len(),
        1,
        "one mount wins the path, not both: {mounts:?}"
    );
    assert_eq!(
        at_path[0].store_id, s2,
        "the carried effective store is the only staged store"
    );

    assert_eq!(
        memory_mount_store_id(&host.sandbox_spec("t-g3").mounts[0]),
        s2
    );
}

/// G4 — an effective Memory input whose backing store is absent fails closed.
#[tokio::test]
async fn a_bound_resource_with_a_missing_backing_store_fails_the_session_closed() {
    use awaken_protocol_managed::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone()).with_resource_configs(Arc::new(
        awaken_config_resolver::InMemoryResourceCatalog::new(),
    ));
    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: "never-seeded-store".into(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        git_ref: None,
    }]);

    let result = managed.prepare_session("t-g4", init).await;
    assert!(
        result.is_err(),
        "a binding to a missing backing store must fail closed, not mount empty"
    );
}

/// G5 — the effective resource reference crosses the node boundary, so a worker needs
/// access to the resource data plane but not to the Agent authoring repository.
#[tokio::test]
async fn an_effective_resource_mounts_on_a_worker_without_the_binding_repository() {
    use awaken_protocol_managed::SessionRuntime;
    let db_less = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = db_less.create_memory_store().await;
    db_less
        .memory_stores
        .fs()
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
        git_ref: None,
    }]);
    managed_worker
        .prepare_session("t-g5-worker", init)
        .await
        .unwrap();
    assert_eq!(
        memory_mount_store_id(&db_less.sandbox_spec("t-g5-worker").mounts[0]),
        store_id,
        "the effective input is sufficient for a DB-less worker"
    );
    assert!(
        db_less.thread_memory_mounts("t-g5-worker").is_empty(),
        "a read-only Memory input is never registered for write-back"
    );
}

#[tokio::test]
async fn ctx_for_carries_one_self_consistent_snapshot_on_any_claiming_node() {
    use awaken_runtime_contract::resolver::RunResolver;

    // A durable run is driven by whichever pool node claims it (ADR-0019). Its
    // session config is already the complete execution authority and resolves
    // without manufacturing a second node-local catalog object.
    let host = SharedHost::new(Arc::new(OkModel), "stub");
    let ctx = host
        .ctx_for("t-catalog", None)
        .await
        .expect("session builds");
    let resolved = ctx.runtime.resolve(&ctx.config).expect("snapshot resolves");
    assert_eq!(resolved.snapshot_id, ctx.config.id);
}

#[tokio::test]
async fn claimed_snapshot_is_the_worker_session_authority() {
    let published = awaken_runtime_contract::ExecutableAgentSnapshot::builder("published-agent")
        .instructions("published instructions")
        .fingerprint("sha256:published")
        .plugin_config([(
            "permission".to_string(),
            serde_json::json!({"default": "deny", "rules": []}),
        )])
        .build();
    let host = SharedHost::new(Arc::new(OkModel), "host-default");

    let ctx = host
        .ctx_for_snapshot_with_sandbox(
            "t-published-claim",
            Some("published-agent"),
            Some(published.clone()),
            None,
        )
        .await
        .expect("worker session builds from claimed snapshot");

    assert_eq!(ctx.config, published);
    assert_eq!(
        ctx.config.resolved_spec.catalog_fingerprint.0,
        "sha256:published"
    );
}

#[tokio::test]
async fn replacement_host_adopts_the_dispatch_sandbox_from_a_stable_root() {
    let storage = tempfile::tempdir().expect("storage dir");
    let thread = "t-sandbox-recovery";

    let first = SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path());
    let first_ctx = first.ctx_for(thread, None).await.expect("first session");
    let handle = first_ctx.env.handle();
    let marker = storage
        .path()
        .join("sandboxes")
        .join(thread)
        .join("recovery-marker");
    std::fs::write(&marker, b"survived").expect("write sandbox marker");
    drop(first_ctx);
    drop(first);

    let replacement = SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path());
    let adopted = replacement
        .provider
        .adopt_sandbox(&handle)
        .await
        .expect("adopt durable handle");
    assert_eq!(
        adopted.status().await.unwrap(),
        awaken_provisioning_contract::SandboxStatus::Ready
    );
    let replacement_ctx = replacement
        .ctx_for_with_sandbox(thread, None, Some(adopted))
        .await
        .expect("replacement session");

    assert_eq!(replacement_ctx.env.handle(), handle);
    assert_eq!(std::fs::read(marker).unwrap(), b"survived");
}

// ---------------------------------------------------------------------------
// run/resume fail-closed boundaries (ADR-0048 gap review)
//
// These guard the double-run / wrong-tool / forged-approval seams: a caller must
// not be able to start a second turn on an awaiting thread, resume a run that never
// awaiting, answer the wrong pending tool, or cross the built-in↔client-executed
// binding when resuming. All of them must fail *closed* with a BadRequest and
// leave the run untouched.
// ---------------------------------------------------------------------------

/// Awaits on the Ask-gated built-in `write` until it sees a tool result, then ends.
struct AwaitOnWriteModel;

#[async_trait::async_trait]
impl LlmExecutor for AwaitOnWriteModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let saw_tool = request.messages.iter().any(|m| m.role == Role::Tool);
        let output = if saw_tool {
            AssistantOutput::text("done")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w1".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({ "path": "note.txt", "content": "x" }),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// Calls the client-executed tool `lookup` until it sees a tool result, then ends
/// by echoing what the result carried — so a test can prove the delivered client
/// result actually reached the model's next inference.
struct ClientLookupModel;

#[async_trait::async_trait]
impl LlmExecutor for ClientLookupModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let tool_text = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(block_text(content)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(",");
        let output = if tool_text.is_empty() {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "c1".into(),
                tool_id: "lookup".into(),
                arguments: serde_json::json!({ "q": "weather" }),
            }])
        } else {
            AssistantOutput::text(format!("result was {tool_text}"))
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

fn user(text: &str) -> Vec<Message> {
    vec![Message::text(MessageId("u1".into()), Role::User, text)]
}

/// A thread awaiting on a tool decision must reject a fresh `run`: starting a second
/// turn over an awaiting run would double-execute the awaiting turn's side effects. The
/// guard fails closed with BadRequest and does not touch the await.
#[tokio::test]
async fn run_on_an_awaiting_thread_fails_closed() {
    let host = SharedHost::new(Arc::new(AwaitOnWriteModel), "stub");
    let r1 = host
        .run(None, "t-awaiting", user("hi"))
        .await
        .expect("turn 1");
    assert!(
        matches!(r1.state, RunState::Awaiting),
        "turn awaits on write"
    );

    let err = host
        .run(None, "t-awaiting", user("again"))
        .await
        .err()
        .expect("a second run on an awaiting thread must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("awaiting a tool decision"),
        "message names the await: {}",
        err.message
    );

    // The await still resumes cleanly afterwards — the rejected run was a no-op.
    let r2 = host
        .resume(
            "t-awaiting",
            "w1",
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
        )
        .await
        .expect("resume the untouched await");
    assert!(matches!(r2.state, RunState::Ended(_)));
}

/// Resuming a thread that has no awaiting run is a caller error, not a panic: there
/// is no run to answer, so it fails closed with BadRequest.
#[tokio::test]
async fn resume_with_no_awaiting_run_fails_closed() {
    let host = SharedHost::new(Arc::new(AwaitOnWriteModel), "stub");
    let err = host
        .resume(
            "t-idle",
            "w1",
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
        )
        .await
        .err()
        .expect("resume with nothing awaiting must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("no awaiting run"),
        "message names the missing await: {}",
        err.message
    );
}

/// A resume whose `tool_use_id` does not name the pending tool must be rejected —
/// otherwise a caller could resume the wrong tool. Fails closed with BadRequest and
/// the real await survives.
#[tokio::test]
async fn resume_with_a_wrong_tool_use_id_fails_closed() {
    let host = SharedHost::new(Arc::new(AwaitOnWriteModel), "stub");
    let r1 = host
        .run(None, "t-wrongid", user("hi"))
        .await
        .expect("turn 1");
    assert!(matches!(r1.state, RunState::Awaiting));

    let err = host
        .resume(
            "t-wrongid",
            "not-the-pending-id",
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
        )
        .await
        .err()
        .expect("a mismatched tool_use_id must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("does not match the pending tool"),
        "message names the mismatch: {}",
        err.message
    );

    // The genuine id still resumes — the mismatch did not consume the await.
    let r2 = host
        .resume(
            "t-wrongid",
            &r1.pending.expect("a pending tool").tool_use_id,
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
        )
        .await
        .expect("the real id resumes");
    assert!(matches!(r2.state, RunState::Ended(_)));
}

/// The built-in↔client binding is enforced on resume: a client-tool *result* may
/// not answer a built-in (Ask-gated) tool. Failing open here would let a caller
/// forge an approval by delivering a fabricated result instead of a decision.
#[tokio::test]
async fn client_result_cannot_answer_a_builtin_tool() {
    let host = SharedHost::new(Arc::new(AwaitOnWriteModel), "stub");
    let r1 = host.run(None, "t-bind1", user("hi")).await.expect("turn 1");
    let pending = r1.pending.expect("awaiting on the built-in write");
    assert!(!pending.client_executed, "write is a built-in tool");

    let err = host
        .resume(
            "t-bind1",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: "forged".into(),
                is_error: false,
            },
        )
        .await
        .err()
        .expect("a client result must not answer a built-in tool");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("built-in"),
        "message names the binding: {}",
        err.message
    );
}

/// The other direction of the binding: a confirmation may not answer a
/// client-executed tool (which expects a result, not a permission decision).
#[tokio::test]
async fn confirm_cannot_answer_a_client_tool() {
    let host = SharedHost::new(Arc::new(ClientLookupModel), "stub")
        .with_client_tools(HashSet::from(["lookup".to_string()]));
    let r1 = host.run(None, "t-bind2", user("hi")).await.expect("turn 1");
    let pending = r1.pending.expect("awaiting on the client tool");
    assert!(pending.client_executed, "lookup is client-executed");

    let err = host
        .resume(
            "t-bind2",
            &pending.tool_use_id,
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
        )
        .await
        .err()
        .expect("a confirmation must not answer a client tool");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("client-executed"),
        "message names the binding: {}",
        err.message
    );
}

/// The happy path for the client-executed binding: a `ClientResult` delivers the
/// caller-run tool's output, it reaches the model's next inference, and the turn
/// ends. Complements the Confirm-only resume path the memory tests already cover.
#[tokio::test]
async fn client_result_delivers_a_client_tool_result_and_ends_the_turn() {
    let host = SharedHost::new(Arc::new(ClientLookupModel), "stub")
        .with_client_tools(HashSet::from(["lookup".to_string()]));
    let r1 = host
        .run(None, "t-client", user("hi"))
        .await
        .expect("turn 1");
    let pending = r1.pending.expect("awaiting on the client tool");

    let r2 = host
        .resume(
            "t-client",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: "sunny".into(),
                is_error: false,
            },
        )
        .await
        .expect("client result resumes");
    assert!(matches!(r2.state, RunState::Ended(_)), "the turn ends");
    let reply = r2
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(
        reply, "result was sunny",
        "the delivered client result reached the model's next inference"
    );
}

/// Superseding a run requires durable ingress; a default (direct-ingress) host
/// must fail closed rather than silently behave like a plain run.
#[tokio::test]
async fn supersede_run_without_durable_ingress_fails_closed() {
    let host = SharedHost::new(Arc::new(AwaitOnWriteModel), "stub");
    // Await first so the supersede path is not short-circuited by the awaiting guard
    // (supersede is allowed on an awaiting thread; the durable check is what must fire).
    let r1 = host.run(None, "t-sup", user("hi")).await.expect("turn 1");
    assert!(matches!(r1.state, RunState::Awaiting));

    let err = host
        .supersede_run(None, "t-sup", user("newest wins"))
        .await
        .err()
        .expect("supersede without durable ingress must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("durable ingress"),
        "message names the requirement: {}",
        err.message
    );
}

/// A terminal session end (managed session delete/archive) disposes the thread's
/// sandbox — the ONLY place it is reaped. Proven end-to-end through the
/// `SessionRuntime` port (`ManagedHost::end_session`): the cached ctx is evicted
/// AND the live sandbox's workspace dir is actually reaped (its `status` flips
/// `Ready` → `Terminated`), unlike the evict-to-rebuild edges (attach/detach/
/// rebind) which keep the per-thread workspace so the next turn reuses it.
#[tokio::test]
async fn end_session_disposes_the_threads_sandbox() {
    use awaken_protocol_managed::SessionRuntime;
    use awaken_provisioning_contract::{Sandbox, SandboxStatus};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone());

    // A first turn builds + caches the thread's sandbox.
    host.run(
        None,
        "t-end",
        vec![Message::text(MessageId("hi".into()), Role::User, "hi")],
    )
    .await
    .expect("first turn");
    // Hold the live sandbox handle before teardown so we can observe its disposal
    // even after the ctx is evicted from the registry.
    let env = host
        .sessions
        .lock()
        .await
        .get("t-end")
        .expect("the first turn caches the thread's sandbox ctx")
        .env
        .clone();
    assert_eq!(
        Sandbox::status(&*env).await.expect("status"),
        SandboxStatus::Ready,
        "the sandbox workspace exists while the session is live"
    );

    // Stage representative resource/config projections after the sandbox is live;
    // terminal cleanup must erase all of them so reusing the opaque thread id cannot
    // inherit stale scope, capability, model, or credential-bearing MCP state.
    host.register_thread_workspace("t-end", "workspace-a");
    host.register_thread_memory("t-end", None);
    host.register_thread_resources("t-end", crate::provisioning::StagedResources::default());
    host.register_thread_mcp(
        "t-end",
        vec![PreparedMcpServer {
            name: "private".into(),
            url: "https://example.invalid/mcp".into(),
            bearer: None,
            refresh: None,
        }],
    );
    host.register_thread_model("t-end", "private-model");
    host.register_thread_egress("t-end", true);

    // End the session at the terminal edge.
    managed.end_session("t-end").await.expect("end_session");

    // The cached ctx is evicted ...
    assert!(
        !host.sessions.lock().await.contains_key("t-end"),
        "end_session evicts the cached ctx"
    );
    // ... and the sandbox is ACTUALLY disposed: its workspace dir was reaped, so a
    // subsequent status reports Terminated (proving dispose ran, not just an evict).
    assert_eq!(
        Sandbox::status(&*env).await.expect("status"),
        SandboxStatus::Terminated,
        "end_session disposes the sandbox (workspace reaped), unlike an evict-rebuild"
    );
    assert!(host.registered_thread_workspace("t-end").is_none());
    assert!(!host.thread_memory.lock().unwrap().contains_key("t-end"));
    assert!(!host.thread_resources.lock().unwrap().contains_key("t-end"));
    assert!(!host.thread_mcp.lock().unwrap().contains_key("t-end"));
    assert!(host.inference_routing.override_for("t-end").is_none());
    assert!(!host.thread_egress().denies("t-end"));

    // Idempotent: ending an already-ended or never-created session is a clean no-op.
    managed
        .end_session("t-end")
        .await
        .expect("end_session is idempotent");
    managed
        .end_session("never-existed")
        .await
        .expect("end_session is a no-op for an unknown thread");
}

struct AwaitRemoteChildModel {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for AwaitRemoteChildModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                call_id: "remote-child-call".into(),
                tool_id: awaken_ext_builtin_tools::AGENT_RUN.into(),
                arguments: serde_json::json!({
                    "agent_id": "researcher",
                    "input": "investigate"
                }),
            }])
        } else {
            AssistantOutput::text("done")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[derive(Default)]
struct RecoverableRemoteChild {
    cancellations: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_runtime_contract::delegation::RemoteAgent for RecoverableRemoteChild {
    async fn run(
        &self,
        _agent_id: &str,
        _request_id: &str,
        _input: &str,
        _cancellation: Option<&CancellationToken>,
    ) -> Result<
        awaken_runtime_contract::delegation::DelegationStep,
        awaken_runtime_contract::delegation::DelegationExecutionError,
    > {
        Ok(
            awaken_runtime_contract::delegation::DelegationStep::Awaiting {
                continuation: serde_json::json!({"task_id": "remote-task-9"}),
            },
        )
    }

    async fn card(
        &self,
        _agent_id: &str,
    ) -> Result<serde_json::Value, awaken_runtime_contract::delegation::DelegationExecutionError>
    {
        Ok(serde_json::json!({"name": "researcher"}))
    }

    async fn cancel(
        &self,
        _agent_id: &str,
        _child_run_id: &RunId,
        execution_reference: Option<&serde_json::Value>,
    ) -> Result<(), awaken_runtime_contract::delegation::DelegationExecutionError> {
        assert_eq!(
            execution_reference.and_then(|value| value["task_id"].as_str()),
            Some("remote-task-9")
        );
        self.cancellations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn rebuilt_host_redelivers_an_awaiting_remote_child_cancellation() {
    let storage = tempfile::tempdir().expect("temporary durable host store");
    let remote = Arc::new(RecoverableRemoteChild::default());
    let host = SharedHost::new(
        Arc::new(AwaitRemoteChildModel {
            calls: AtomicUsize::new(0),
        }),
        "stub",
    )
    .with_store_dir(storage.path())
    .with_remote_agent("researcher", remote.clone());

    let result = host
        .run(None, "cancel-recovery", user("start child"))
        .await
        .expect("parent awaits remote child");
    assert_eq!(result.state, RunState::Awaiting);
    let ctx = host
        .ctx_for("cancel-recovery", None)
        .await
        .expect("session context");
    let parent_run_id = ctx
        .state
        .lock()
        .await
        .awaiting_run
        .clone()
        .expect("awaiting parent id");
    ctx.runtime
        .cancel_run(
            parent_run_id,
            ctx.thread_id.clone(),
            RuntimeRunContext::new()
                .with_commit(ctx.commit.clone())
                .with_reader(ctx.commit.clone()),
        )
        .await
        .expect("terminal parent commit and first cancellation delivery");
    assert_eq!(remote.cancellations.load(Ordering::SeqCst), 1);

    drop(ctx);
    drop(host);
    let replacement = SharedHost::new(Arc::new(OkModel), "stub")
        .with_store_dir(storage.path())
        .with_remote_agent("researcher", remote.clone());
    replacement.committed_messages("cancel-recovery").await;
    assert_eq!(
        remote.cancellations.load(Ordering::SeqCst),
        2,
        "a new process has no ephemeral receipt and redelivers the durable intent"
    );
}
