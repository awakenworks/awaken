use super::*;
use crate::config::block_text;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
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
        workspace_id: host.local_workspace().into(),
        agent_id: "a".into(),
        mcp_servers: Vec::new(),
        resources: Vec::new(),
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
        agent_id: "a".into(),
        mcp_servers: Vec::new(),
        resources: Vec::new(),
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
        agent_id: "a".into(),
        mcp_servers: Vec::new(),
        resources: Vec::new(),
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
        .blob()
        .put(
            host.local_workspace(),
            &store_id,
            b"the secret code is BANANA-42",
        )
        .await
        .expect("seed memory bytes");
    let bindings: Arc<dyn ResourceStore> = Arc::new(InMemoryResourceStore::new());
    bindings.put_agent_resource_in(
        host.local_workspace(),
        AgentResourceConfig {
            agent_id: "a".into(),
            resources: vec![ResourceBinding {
                kind: ResourceKind::MemoryStore,
                resource_id: store_id.clone(),
                mount_path: "/mnt/memory".into(),
                access: ResourceAccess::ReadWrite,
                instructions: None,
            }],
            version: 1,
        },
    );
    let managed = crate::ManagedHost::new(host.clone()).with_resources(bindings);

    let bare = |agent: &str| SessionInit {
        workspace_id: host.local_workspace().into(),
        agent_id: agent.into(),
        mcp_servers: Vec::new(),
        resources: Vec::new(),
        model: None,
        runtime: None,
        deny_egress: false,
        sandbox: None,
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
    host.grant_file(host.local_workspace(), &file_id);
    let bindings: Arc<dyn ResourceStore> = Arc::new(InMemoryResourceStore::new());
    bindings.put_agent_resource_in(
        host.local_workspace(),
        AgentResourceConfig {
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
        },
    );
    let managed = crate::ManagedHost::new(host.clone()).with_resources(bindings);

    managed
        .prepare_session(
            "t-multi",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                mcp_servers: Vec::new(),
                resources: Vec::new(),
                model: None,
                runtime: None,
                deny_egress: false,
                sandbox: None,
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

/// Managed-Agents model: a `github_repository` session resource clones host-side AND injects
/// a scoped `github:<logical>` MCP server whose token is held host-side — so the agent drives
/// branch/commit/push/PR through MCP tools while the credential never enters the sandbox.
#[tokio::test]
async fn a_github_repository_resource_injects_a_scoped_github_mcp_server() {
    use awaken_protocol_managed::{SessionInit, SessionResource, SessionRuntime};
    use awaken_run_executor_acp::McpCredential;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone()).with_mcp(
        Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        Arc::new(awaken_config_resolver::InMemoryMcpStore::new()),
    );

    managed
        .prepare_session(
            "t-gh",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                mcp_servers: Vec::new(),
                resources: vec![SessionResource {
                    kind: "github_repository".into(),
                    id: "https://github.com/awaken/example.git".into(),
                    mount_path: "/workspace/repo".into(),
                    instructions: None,
                    auth_token: Some("ghp_secret_token".into()),
                    git_ref: None,
                }],
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

/// Managed-Agents `resources.update`: rotating a github_repository's authorization token
/// re-keys BOTH the staged clone token and the injected GitHub MCP bearer, host-side.
#[tokio::test]
async fn rotating_a_github_repository_token_re_keys_the_clone_and_mcp_bearer() {
    use awaken_protocol_managed::{SessionInit, SessionResource, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone()).with_mcp(
        Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        Arc::new(awaken_config_resolver::InMemoryMcpStore::new()),
    );
    let gh_res = |token: &str| SessionResource {
        kind: "github_repository".into(),
        id: "https://github.com/awaken/example.git".into(),
        mount_path: "/workspace/repo".into(),
        instructions: None,
        auth_token: Some(token.into()),
        git_ref: None,
    };
    managed
        .prepare_session(
            "t-rot",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                mcp_servers: Vec::new(),
                resources: vec![gh_res("ghp_old")],
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
            .token
            .as_ref()
            .map(|t| t.expose_secret().to_string())
    };
    assert_eq!(mcp_bearer(&host).as_deref(), Some("ghp_old"));
    assert_eq!(clone_token(&host).as_deref(), Some("ghp_old"));

    // Rotate the token (POST /v1/sessions/{id}/resources/{rid} with a new authorization_token).
    managed
        .rotate_resource_token("t-rot", gh_res("ghp_new"))
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
// Resource-plane seam coverage (ADR-0038): the gaps a per-layer test misses.
//   G1 consistency  — what the config plane TELLS the agent == what the host MOUNTS
//   G3 decision     — a per-session wire resource overrides the agent binding
//   G4 fail-closed  — a bound resource with a missing backing aborts session prep
//   G5 gap          — without the binding store (a db-less worker) nothing mounts
// ---------------------------------------------------------------------------

/// A bare session for `agent` with no wire resources — the common "just run the agent"
/// path where only its bound resources apply.
#[cfg(test)]
fn bare_session(agent: &str, workspace: &str) -> awaken_protocol_managed::SessionInit {
    awaken_protocol_managed::SessionInit {
        workspace_id: workspace.into(),
        agent_id: agent.into(),
        mcp_servers: Vec::new(),
        resources: Vec::new(),
        model: None,
        runtime: None,
        deny_egress: false,
        sandbox: None,
    }
}

/// G1 — the "told == mounted" invariant. The SAME `ResourceStore` the config plane
/// renders into the agent's prompt is what the host mounts, so the realized path the
/// agent is TOLD to read (`resource_prompts_for`, the compile side) is exactly where
/// the bytes are MOUNTED (`sandbox_spec`, the run side), and the access agrees. Both
/// sides are tested in isolation elsewhere; this pins that they agree from one binding.
#[tokio::test]
async fn told_equals_mounted_the_prompt_path_and_access_match_the_realized_mount() {
    use awaken_config_resolver::{
        AgentResourceConfig, InMemoryResourceStore, ResourceAccess, ResourceBinding, ResourceKind,
        ResourceStore, realized_mount_path, resource_prompts_for,
    };
    use awaken_protocol_managed::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = host.create_memory_store().await;
    host.memory_stores
        .blob()
        .put(host.local_workspace(), &store_id, b"seed")
        .await
        .expect("seed memory bytes");

    let cfg = AgentResourceConfig {
        agent_id: "a".into(),
        resources: vec![ResourceBinding {
            kind: ResourceKind::MemoryStore,
            resource_id: store_id.clone(),
            mount_path: "/mnt/memory".into(),
            access: ResourceAccess::ReadWrite,
            instructions: None,
        }],
        version: 1,
    };
    let bindings: Arc<dyn ResourceStore> = Arc::new(InMemoryResourceStore::new());
    bindings.put_agent_resource_in(host.local_workspace(), cfg.clone());
    let managed = crate::ManagedHost::new(host.clone()).with_resources(bindings);
    managed
        .prepare_session("t-g1", bare_session("a", host.local_workspace()))
        .await
        .unwrap();

    let binding = &cfg.resources[0];
    // The realized path the config plane names in the prompt.
    let realized = realized_mount_path(binding.kind, &binding.mount_path); // ".mnt/mnt/memory"
    let prompts = resource_prompts_for(&cfg);
    assert!(
        prompts.iter().any(|p| p.contains(&realized)),
        "the compiled prompt names the realized path {realized}: {prompts:?}"
    );
    assert!(
        prompts.iter().any(|p| p.contains("read/write")),
        "a read/write binding is described read/write: {prompts:?}"
    );
    // The mount the host realizes: its logical_path, under `.mnt/`, is that same path.
    let logical = binding.mount_path.trim_start_matches('/'); // "mnt/memory"
    assert_eq!(
        realized,
        format!(".mnt/{logical}"),
        "realized-path contract"
    );
    let dump = serde_json::to_string(&host.sandbox_spec("t-g1").mounts).unwrap();
    assert!(
        dump.contains(&format!("\"mount_path\":\"{realized}\"")),
        "the host mounts at exactly the path the prompt named: {dump}"
    );
}

/// G3 — the staging decision table: a per-session wire resource on the SAME mount_path
/// as an agent binding WINS (an explicit override beats the default binding). Cause:
/// agent-bound store S1 @ /mnt/memory AND wire store S2 @ /mnt/memory. Effect: exactly
/// one memory mount at that path, carrying S2's bytes, never S1's.
#[tokio::test]
async fn a_wire_resource_overrides_the_agent_binding_at_the_same_path() {
    use awaken_config_resolver::{
        AgentResourceConfig, InMemoryResourceStore, ResourceAccess, ResourceBinding, ResourceKind,
        ResourceStore,
    };
    use awaken_protocol_managed::{SessionResource, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let s1 = host.create_memory_store().await; // agent-bound
    let s2 = host.create_memory_store().await; // wire override
    host.memory_stores
        .blob()
        .put(host.local_workspace(), &s1, b"AGENT-BYTES")
        .await
        .unwrap();
    host.memory_stores
        .blob()
        .put(host.local_workspace(), &s2, b"WIRE-BYTES")
        .await
        .unwrap();

    let bindings: Arc<dyn ResourceStore> = Arc::new(InMemoryResourceStore::new());
    bindings.put_agent_resource_in(
        host.local_workspace(),
        AgentResourceConfig {
            agent_id: "a".into(),
            resources: vec![ResourceBinding {
                kind: ResourceKind::MemoryStore,
                resource_id: s1.clone(),
                mount_path: "/mnt/memory".into(),
                access: ResourceAccess::ReadWrite,
                instructions: None,
            }],
            version: 1,
        },
    );
    let managed = crate::ManagedHost::new(host.clone()).with_resources(bindings);

    let mut init = bare_session("a", host.local_workspace());
    init.resources = vec![SessionResource {
        kind: "memory_store".into(),
        id: s2.clone(),
        mount_path: "/mnt/memory".into(), // SAME path as the agent binding
        instructions: None,
        auth_token: None,
        git_ref: None,
    }];
    managed.prepare_session("t-g3", init).await.unwrap();

    // Exactly one memory mount at that path, and it is the wire store S2.
    let mounts = host.thread_memory_mounts("t-g3");
    let at_path: Vec<_> = mounts
        .iter()
        .filter(|(_, logical)| logical == "mnt/memory")
        .collect();
    assert_eq!(
        at_path.len(),
        1,
        "one mount wins the path, not both: {mounts:?}"
    );
    assert_eq!(
        at_path[0].0, s2,
        "the wire store overrides the agent binding"
    );

    let dump = serde_json::to_string(&host.sandbox_spec("t-g3").mounts).unwrap();
    assert!(
        dump.contains("WIRE-BYTES"),
        "the wire bytes are mounted: {dump}"
    );
    assert!(
        !dump.contains("AGENT-BYTES"),
        "the overridden binding is not mounted: {dump}"
    );
}

/// G4 — fail-closed. An agent bound to a memory_store whose backing store does NOT
/// exist must abort session prep, never run believing an absent mount is real
/// (symmetric to the file / no-mounter fail-closed already covered at other layers).
#[tokio::test]
async fn a_bound_resource_with_a_missing_backing_store_fails_the_session_closed() {
    use awaken_config_resolver::{
        AgentResourceConfig, InMemoryResourceStore, ResourceAccess, ResourceBinding, ResourceKind,
        ResourceStore,
    };
    use awaken_protocol_managed::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let bindings: Arc<dyn ResourceStore> = Arc::new(InMemoryResourceStore::new());
    bindings.put_agent_resource_in(
        host.local_workspace(),
        AgentResourceConfig {
            agent_id: "a".into(),
            resources: vec![ResourceBinding {
                kind: ResourceKind::MemoryStore,
                resource_id: "never-seeded-store".into(), // no backing bytes exist
                mount_path: "/mnt/memory".into(),
                access: ResourceAccess::ReadWrite,
                instructions: None,
            }],
            version: 1,
        },
    );
    let managed = crate::ManagedHost::new(host.clone()).with_resources(bindings);

    let result = managed
        .prepare_session("t-g4", bare_session("a", host.local_workspace()))
        .await;
    assert!(
        result.is_err(),
        "a binding to a missing backing store must fail closed, not mount empty"
    );
}

/// G5 — characterization of the known cross-node gap, as a CONTRAST that isolates the
/// gap to one variable. The SAME agent binding (same seeded memory store, same
/// `AgentResourceConfig`) mounts on a host that HAS the `ResourceStore` (the all-in-one
/// / co-located case) but NOT on a host without it (a database-less remote worker —
/// which drives from a snapshot carrying only rendered instructions, never the binding
/// store or structured mounts). The only difference between the two is whether the
/// `ResourceStore` crossed the node boundary, so this pins the gap precisely: it is
/// "the store is not carried cross-node", not "no binding exists". When resources ARE
/// carried to a worker, the second half flips and this test must be updated.
#[tokio::test]
async fn an_agents_bound_resource_mounts_with_the_store_but_not_on_a_db_less_worker() {
    use awaken_config_resolver::{
        AgentResourceConfig, InMemoryResourceStore, ResourceAccess, ResourceBinding, ResourceKind,
        ResourceStore,
    };
    use awaken_protocol_managed::SessionRuntime;

    // Author ONE binding: agent `a` → a seeded memory store at /mnt/memory. Shared
    // verbatim by both hosts so the ONLY variable is whether the store is present.
    async fn bind(host: &Arc<SharedHost>) -> Arc<dyn ResourceStore> {
        let store_id = host.create_memory_store().await;
        host.memory_stores
            .blob()
            .put(host.local_workspace(), &store_id, b"CARRIED-BYTES")
            .await
            .expect("seed");
        let bindings: Arc<dyn ResourceStore> = Arc::new(InMemoryResourceStore::new());
        bindings.put_agent_resource_in(
            host.local_workspace(),
            AgentResourceConfig {
                agent_id: "a".into(),
                resources: vec![ResourceBinding {
                    kind: ResourceKind::MemoryStore,
                    resource_id: store_id,
                    mount_path: "/mnt/memory".into(),
                    access: ResourceAccess::ReadWrite,
                    instructions: None,
                }],
                version: 1,
            },
        );
        bindings
    }

    // Positive control: a host that HAS the ResourceStore mounts the bound resource.
    let with_store = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let bindings = bind(&with_store).await;
    let managed_with = crate::ManagedHost::new(with_store.clone()).with_resources(bindings);
    managed_with
        .prepare_session("t-g5-with", bare_session("a", with_store.local_workspace()))
        .await
        .unwrap();
    let with_dump = serde_json::to_string(&with_store.sandbox_spec("t-g5-with").mounts).unwrap();
    assert!(
        with_dump.contains("CARRIED-BYTES"),
        "with the store, the agent's bound resource mounts: {with_dump}"
    );
    assert!(
        !with_store.thread_memory_mounts("t-g5-with").is_empty(),
        "with the store, a memory mount is staged"
    );

    // The gap: a db-less worker (same agent, but NO ResourceStore crossed the boundary)
    // mounts nothing — the binding is invisible to it.
    let db_less = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    bind(&db_less).await; // seed the store's *bytes*, but do NOT wire the binding store
    let managed_worker = crate::ManagedHost::new(db_less.clone()); // no .with_resources(...)
    managed_worker
        .prepare_session("t-g5-worker", bare_session("a", db_less.local_workspace()))
        .await
        .unwrap();
    let worker_dump = serde_json::to_string(&db_less.sandbox_spec("t-g5-worker").mounts).unwrap();
    assert_eq!(
        worker_dump, "[]",
        "the same binding is invisible to a db-less worker (gap): {worker_dump}"
    );
    assert!(
        db_less.thread_memory_mounts("t-g5-worker").is_empty(),
        "no memory mount is staged without the resource store crossing the boundary"
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
