use super::*;
use crate::config::block_text;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, ChatRole};
use std::sync::atomic::AtomicUsize;

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

/// The main assistant answers plainly; the memory extractor (identified by its
/// system instructions) saves one memory then reports done.
struct MemoryHostModel;

#[async_trait::async_trait]
impl LlmExecutor for MemoryHostModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::{ChatRole, ToolCall};
        let is_extractor = request.messages.iter().any(|m| {
            m.role == ChatRole::System
                && m.content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("memory extraction sub-agent"),
                    _ => false,
                })
        });
        let output = if is_extractor {
            if request.messages.iter().any(|m| m.role == ChatRole::Tool) {
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
            .filter(|m| m.role == ChatRole::System)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let reply = if system_text.contains("conversation-compaction sub-agent") {
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
    assert!(matches!(r1.phase, Phase::Ended(_)));
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
        use awaken_runtime_contract::llm::{ChatRole, ToolCall};
        let system_text: String = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::System)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if system_text.contains("memory extraction sub-agent") {
            let already = request.messages.iter().any(|m| {
                m.role == ChatRole::Tool
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

/// The main agent parks on a `write` (Ask-gated) then finishes on resume; the
/// extractor saves a memory. Proves resume-ended turns trigger the aux agents.
struct ResumeMemModel;

#[async_trait::async_trait]
impl LlmExecutor for ResumeMemModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::{ChatRole, ToolCall};
        let saw_tool = request.messages.iter().any(|m| m.role == ChatRole::Tool);
        // The extractor's own write_memory succeeded (its result text), distinct
        // from the main turn's `write` result that is also in its seeded context.
        let saved_memory = request.messages.iter().any(|m| {
            m.role == ChatRole::Tool
                && m.content.iter().any(|b| match b {
                    ContentBlock::ToolResult { content, .. } => {
                        block_text(content).contains("saved memory")
                    }
                    _ => false,
                })
        });
        let is_extractor = request.messages.iter().any(|m| {
            m.role == ChatRole::System
                && m.content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("memory extraction sub-agent"),
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

    // Turn 1 parks on the Ask-gated `write`.
    let r1 = host
        .run(
            None,
            "t-res",
            vec![Message::text(MessageId("u1".into()), Role::User, "hi")],
        )
        .await
        .expect("turn 1");
    assert!(
        matches!(r1.phase, Phase::Waiting),
        "turn should park on write"
    );
    let pending = r1.pending.expect("a pending tool");

    // Resume approves the write; the turn now ends and extraction fires.
    let r2 = host
        .resume(
            "t-res",
            &pending.tool_use_id,
            HostResume::Confirm {
                allow: true,
                note: None,
            },
        )
        .await
        .expect("resume");
    assert!(
        matches!(r2.phase, Phase::Ended(_)),
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
        use awaken_runtime_contract::llm::{ChatRole, ToolCall};
        let is_extractor = request.messages.iter().any(|m| {
            m.role == ChatRole::System
                && m.content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("memory extraction sub-agent"),
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
        if request.messages.iter().any(|m| m.role == ChatRole::Tool) {
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
            .filter(|m| m.role == ChatRole::User)
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
    assert!(matches!(result.phase, Phase::Ended(_)), "turn should end");

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
async fn attach_resource_stages_the_mount_and_evicts_the_cached_sandbox() {
    use awaken_protocol_managed::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone());
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    // A blob to mount, and a first turn that builds + caches the thread's sandbox.
    let file_id = host
        .file_store()
        .put(b"hello-attached")
        .await
        .expect("put blob");
    host.run(None, "t-attach", user("hi"))
        .await
        .expect("first turn");
    assert!(
        host.sessions.lock().await.contains_key("t-attach"),
        "the first turn caches the thread's sandbox ctx"
    );
    let before = host.sandbox_spec("t-attach").mounts.len();

    // Attach a file resource on the LIVE session.
    let res = awaken_protocol_managed::SessionResource {
        kind: "file".into(),
        id: file_id,
        mount_path: "/data.txt".into(),
        instructions: None,
        auth_token: None,
        git_ref: None,
    };
    managed
        .attach_resource("t-attach", res.clone())
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
    assert!(
        dump.contains("hello-attached"),
        "mount carries the file's bytes"
    );

    // Detach removes exactly that mount again.
    managed
        .detach_resource("t-attach", res)
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
    let managed = crate::ManagedHost::new(host.clone());
    let init = |deny: bool| SessionInit {
        agent_id: "a".into(),
        mcp_servers: Vec::new(),
        resources: Vec::new(),
        model: None,
        runtime: None,
        deny_egress: deny,
    };

    managed.prepare_session("t-deny", init(true)).await.unwrap();
    assert!(
        host.sandbox_spec("t-deny").deny_egress,
        "a deny_egress session stages into the sandbox spec"
    );

    managed
        .prepare_session("t-open", init(false))
        .await
        .unwrap();
    assert!(
        !host.sandbox_spec("t-open").deny_egress,
        "an unrestricted session keeps the host network"
    );
}

/// A published agent's bound memory store (ADR-0038) is mounted in EVERY session it
/// runs — not only sessions that pass it as a wire `resource`. With the binding store
/// shared (`with_resources`), a BARE `prepare_session` for the agent stages the store's
/// mount, carrying its bytes, at the binding's path. This is the seam that makes the
/// build→bind→use loop real: the config plane injects the prompt, this injects the mount.
#[tokio::test]
async fn prepare_session_mounts_the_agents_bound_memory_store() {
    use awaken_config_resolver::{
        AgentResourceConfig, InMemoryResourceStore, ResourceAccess, ResourceBinding, ResourceKind,
        ResourceStore,
    };
    use awaken_protocol_managed::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));

    // Seed a memory store with a known secret, and bind it to agent `a` at /mnt/memory.
    let store_id = host.create_memory_store().await;
    host.memory_stores
        .put(
            crate::provisioning::HOST_MEMORY_WORKSPACE,
            &store_id,
            b"the secret code is BANANA-42",
        )
        .await
        .expect("seed memory bytes");
    let bindings: Arc<dyn ResourceStore> = Arc::new(InMemoryResourceStore::new());
    bindings.put_agent_resource(AgentResourceConfig {
        agent_id: "a".into(),
        resources: vec![ResourceBinding {
            kind: ResourceKind::MemoryStore,
            resource_id: store_id.clone(),
            mount_path: "/mnt/memory".into(),
            access: ResourceAccess::ReadWrite,
            instructions: None,
        }],
        version: 1,
    });
    let managed = crate::ManagedHost::new(host.clone()).with_resources(bindings);

    let bare = |agent: &str| SessionInit {
        agent_id: agent.into(),
        mcp_servers: Vec::new(),
        resources: Vec::new(),
        model: None,
        runtime: None,
        deny_egress: false,
    };

    // A BARE session for the bound agent — no wire resources at all — still mounts it.
    managed.prepare_session("t-bound", bare("a")).await.unwrap();
    let dump = serde_json::to_string(&host.sandbox_spec("t-bound").mounts).unwrap();
    assert!(
        dump.contains("mnt/memory"),
        "the bound memory store is mounted at its path: {dump}"
    );
    assert!(
        dump.contains("BANANA-42"),
        "the mount carries the store's bytes: {dump}"
    );

    // An agent with NO binding gets nothing extra — the loop is opt-in per binding.
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

/// The same auto-attach realizes an agent's other bound resource kinds (ADR-0038): a
/// bound FILE is mounted from the blob store carrying its bytes, and a bound
/// github_repository is staged for the host-side clone (a repo is not a byte mount, so
/// it's observed via the repo stage rather than `sandbox_spec`).
#[tokio::test]
async fn prepare_session_mounts_bound_file_and_stages_bound_repo() {
    use awaken_config_resolver::{
        AgentResourceConfig, InMemoryResourceStore, ResourceAccess, ResourceBinding, ResourceKind,
        ResourceStore,
    };
    use awaken_protocol_managed::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));

    // Seed a file blob, and bind BOTH a file and a repo to agent `a`.
    let file_id = host
        .file_store()
        .put(b"port is 8080")
        .await
        .expect("put blob");
    let bindings: Arc<dyn ResourceStore> = Arc::new(InMemoryResourceStore::new());
    bindings.put_agent_resource(AgentResourceConfig {
        agent_id: "a".into(),
        resources: vec![
            ResourceBinding {
                kind: ResourceKind::File,
                resource_id: file_id.clone(),
                mount_path: "/mnt/files/notes.txt".into(),
                access: ResourceAccess::ReadOnly,
                instructions: None,
            },
            ResourceBinding {
                kind: ResourceKind::GithubRepository,
                resource_id: "https://github.com/awaken/example.git".into(),
                mount_path: "/mnt/repo".into(),
                access: ResourceAccess::ReadOnly,
                instructions: None,
            },
        ],
        version: 1,
    });
    let managed = crate::ManagedHost::new(host.clone()).with_resources(bindings);

    managed
        .prepare_session(
            "t-multi",
            SessionInit {
                agent_id: "a".into(),
                mcp_servers: Vec::new(),
                resources: Vec::new(),
                model: None,
                runtime: None,
                deny_egress: false,
            },
        )
        .await
        .unwrap();

    // The file is a byte mount carrying its content, at its path.
    let dump = serde_json::to_string(&host.sandbox_spec("t-multi").mounts).unwrap();
    assert!(
        dump.contains("files/notes.txt"),
        "bound file mounted at its path: {dump}"
    );
    assert!(
        dump.contains("port is 8080"),
        "file mount carries its bytes: {dump}"
    );

    // The repo is staged for a host-side clone (not a byte mount).
    let repos = host.thread_repos("t-multi");
    assert_eq!(repos.len(), 1, "the bound repo is staged for cloning");
    assert_eq!(repos[0].url, "https://github.com/awaken/example.git");
}

#[tokio::test]
async fn ctx_for_installs_a_catalog_so_any_node_can_resolve_a_claimed_run() {
    use awaken_runtime_contract::capability::RuntimeCapabilitySource;

    // A durable run is driven by whichever pool node CLAIMS it (ADR-0019), calling
    // `Runtime::execute` directly on a session runtime built by `ctx_for` — never the
    // in-process `prepare` that installs the catalog on the submitting node. So the
    // catalog must be installed by `ctx_for` itself, or `resolve` fails closed with
    // NoActiveCatalog and the claimed run strands. Assert the session runtime carries
    // an active catalog (a non-empty fingerprint) straight out of `ctx_for`.
    let host = SharedHost::new(Arc::new(OkModel), "stub");
    let ctx = host.ctx_for("t-catalog", None).await.expect("session builds");
    let caps = ctx.runtime.runtime_capabilities();
    assert!(
        !caps.catalog_fingerprint.0.trim().is_empty(),
        "ctx_for must install a catalog on the session runtime (fingerprint was empty)"
    );
}
