//! Compaction windows and automatic Memory recall/extraction across threads.

use super::*;

fn memory_test_snapshot(agent: &str) -> awaken_runtime_contract::ExecutableAgentSnapshot {
    crate::config::server_config(
        agent,
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()],
        &std::collections::BTreeMap::from([(
            awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
            serde_json::json!({"binding_id": "test-input-0"}),
        )]),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .expect("literal model reference")
}

async fn memory_test_host(model: Arc<dyn LlmExecutor>, agent: &str) -> Arc<SharedHost> {
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([memory_test_snapshot(
            agent,
        )])
        .expect("valid Memory Agent publication");
    (crate::host::with_test_resource_validation(with_test_memory_mounter(
        crate::HostDependencies::ephemeral(model, "stub")
            .await
            .with_agent_publications(Arc::new(publications)),
    )))
    .build()
    .expect("validated Host dependencies")
}

/// Install one complete Memory-enabled Session through the immutable Agent and
/// exact Resource-transition authorities. C1 the Host publication source owns
/// the Agent selecting binding `test-input-0`; C2 the Session pins that one
/// writable store. C1+C2 produces one automatic Memory binding used by recall
/// and terminal extraction, without a second publication or slot-write path.
/// Return that same creation-time manifest so a cold child test can freeze its
/// dispatch before any process-local Runtime slot derives a cached copy.
async fn install_complete_memory_test_session(
    managed: &crate::ManagedHost,
    thread: &str,
    agent: &str,
    store: &str,
) -> awaken_session_resource_contract::SessionResourceManifest {
    let resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store.into(),
        mount_path: "/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    let mut init = bare_session(agent, managed.host.local_workspace());
    init.resources = resources.clone();
    let manifest = awaken_session_resource_contract::SessionResourceManifest::new(
        managed.host.local_workspace(),
        resources.clone(),
    );
    install_complete_test_session(managed, thread, init)
        .await
        .expect("install complete Memory Session projection");
    awaken_session_execution_contract::EnvironmentRuntime::apply_session_inputs(
        managed,
        thread,
        &resource_transition(
            managed.host.local_workspace(),
            0,
            Default::default(),
            0,
            resources,
        ),
    )
    .await
    .expect("compile exact Memory Resource transition");
    manifest
}

/// The compactor (identified by its instructions) replies with a fixed summary;
/// the main assistant reports whether it saw a delivered summary in its system
/// messages, proving the summary reached the next Run's model input.
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
            format!(
                "no-summary-users={}",
                request
                    .messages
                    .iter()
                    .filter(|message| message.role == Role::User)
                    .count()
            )
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn compaction_summary_reaches_the_same_long_run() {
    // Test design. Causes: C1 first turn is below compaction threshold; C2 the
    // second turn crosses it on the same Thread. Effects: E1 C1 has no summary;
    // E2 C2 injects the committed summary into that Run's inference. Constraint/
    // Invariant: compaction changes context projection, not Thread identity or
    // committed history. Decision rule: execute C1 then C2 and distinguish replies.
    // C3 the Host supplies request-only environment context on both attempts:
    // it must remain model-visible while contributing no conversational prefix
    // to compaction. C1+C3 => still E1; C2+C3 => E2 over actual Thread history.
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(CompactHostModel), "stub")
            .await
            .with_compaction_tokens(1, 1))
        .build()
        .expect("validated Host dependencies"),
    );
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, "hello")];

    // Run 1: only the single user message → below threshold, no summary injected.
    let r1 = host.run(None, "t-c", user("u1")).await.expect("Run 1");
    assert!(matches!(r1.state, RunState::Ended(_)));
    let reply1 = r1
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(reply1, "no-summary-users=1", "short Run is not compacted");

    // Run 2: the conversation now exceeds the threshold, so the compact plugin's
    // BeforeInference hook summarizes the older slice inline and the model sees it.
    let r2 = host.run(None, "t-c", user("u2")).await.expect("Run 2");
    let reply2 = r2
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(reply2, "seen-summary");
}

#[tokio::test]
async fn compaction_keeps_full_history_until_a_summary_activates_the_window() {
    // Test design. Causes: C1 multiple turns remain below the configured summary
    // threshold. Effects: E1 the model sees all user history and no KeepLast fold
    // activates. Constraint/Invariant: window truncation requires committed prefix
    // coverage from a summary. Decision rule: keep C1 false for summary activation
    // and require both user messages remain visible.
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(CompactHostModel), "stub")
            .await
            .with_compaction_tokens(100, 1))
        .build()
        .expect("validated Host dependencies"),
    );
    let user = |id: &str| vec![Message::text(MessageId(id.into()), Role::User, id)];

    host.run(None, "t-before-fold", user("u1"))
        .await
        .expect("Run 1");
    let run = host
        .run(None, "t-before-fold", user("u2"))
        .await
        .expect("Run 2");
    let reply = run
        .new_messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| block_text(&message.content))
        .unwrap_or_default();
    assert_eq!(
        reply, "no-summary-users=2",
        "KeepLast must remain inactive until compaction supplies prefix coverage"
    );
}

/// Durable compaction dispatch cause/effect graph: C1 an embedded Agent enables
/// compaction with a non-default effective token window; C2 reservation generates the
/// immutable dispatch snapshot; C3 a claimed Worker rebuilds exclusively from
/// that snapshot. Effects: E1 C1+C2 freezes the exact plugin id/config; E2
/// C1+C2+C3 retains the same window and tail. Constraint: the generic
/// `plugin_ids`/`plugin_config` projection is the sole configuration authority;
/// no process-local compaction settings are consulted after publication.
///
/// | Rule | configured | phase | window/keep_last | effect |
/// |---|---|---|---|---|
/// | C7 | 2/1 | reservation | 2/1 | freeze exact config |
/// | C8 | 2/1 | claimed rebuild | 2/1 | preserve exact config |
#[tokio::test]
async fn generated_compaction_config_survives_claimed_rebuild() {
    let host = (crate::host::with_test_resource_validation(
        crate::HostDependencies::ephemeral(Arc::new(CompactHostModel), "stub")
            .await
            .with_compaction_tokens(2, 1),
    ))
    .build()
    .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);

    let provisional = host
        .ctx_for_session_coordination("durable-compact-config", Some("assistant"))
        .await
        .expect("C7 reservation context");
    let assert_exact_config = |snapshot: &ExecutableAgentSnapshot, rule: &str| {
        assert!(
            snapshot
                .resolved_spec
                .plugin_ids
                .iter()
                .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID),
            "{rule} selects compact"
        );
        let config: awaken_ext_compact::CompactConfig = serde_json::from_value(
            snapshot.resolved_spec.plugin_config[awaken_ext_compact::COMPACT_PLUGIN_ID].clone(),
        )
        .expect("valid frozen CompactConfig");
        assert_eq!(
            (config.max_tokens, config.keep_last),
            (Some(2), 1),
            "{rule}"
        );
    };
    assert_exact_config(&provisional.config, "C7/E1");

    host.realizer
        .evict_session_for_rebuild("durable-compact-config")
        .await;
    let executable = host
        .ctx_for_snapshot(
            "durable-compact-config",
            Some("assistant"),
            Some(provisional.config.clone()),
        )
        .await
        .expect("C8 claimed-style context");
    assert_exact_config(&executable.config, "C8/E2");
}

#[tokio::test]
async fn published_compact_plugin_is_installed_from_the_immutable_agent() {
    // Cause/effect graph: C1 an immutable publication selects `compact`; C2 the
    // process has no ambient compaction override. C1+C2 must install the exact
    // publication-selected plugin before inference. Otherwise authoring accepts
    // a capability that the product runtime can never execute.
    let snapshot = crate::config::server_config(
        "published-compact",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[awaken_ext_compact::COMPACT_PLUGIN_ID.to_string()],
        &std::collections::BTreeMap::from([(
            awaken_ext_compact::COMPACT_PLUGIN_ID.to_string(),
            serde_json::json!({}),
        )]),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .expect("literal model reference");
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot.clone()])
            .expect("one immutable published Agent");
    let (host, managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_agent_publications(Arc::new(publications)))
        .build()
        .expect("validated Host dependencies"),
    );
    install_test_session_application(&host);
    crate::host::worker_resolver::test_support::install_complete_projection_for_snapshot(
        &managed,
        "published-compact-thread",
        host.local_workspace(),
        session_environment(
            awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &snapshot,
    )
    .await;

    let outcome = run_prepared_session_messages(
        &managed,
        "published-compact",
        "published-compact-thread",
        vec![Message::text(
            MessageId("published-compact-input".into()),
            Role::User,
            "prepare the handoff",
        )],
    )
    .await
    .expect("publication-selected compact plugin runs without ambient enablement");

    assert_eq!(outcome.state(), &RunState::Ended(EndCause::NaturalEnd));
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
    // Test design. Causes: C1 Thread A extracts a durable memory into a shared
    // store; C2 fresh Thread B binds that store and runs. Effects: E1 C1 persists
    // the preference; E2 C2 recalls and uses it. Constraint/Invariant: transfer is
    // only through the Memory store, never copied Thread transcript. Decision rule:
    // write on A, drain extraction, then require recall-derived output on B.
    let host = memory_test_host(Arc::new(MemLoopModel), "memory-agent").await;
    let managed = managed_with_resource_source(host.clone());
    let store = test_memory_store_id();
    install_complete_memory_test_session(&managed, "thread-1", "memory-agent", &store).await;
    install_complete_memory_test_session(&managed, "thread-2", "memory-agent", &store).await;
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    // Thread 1: the user states a preference; extraction saves it.
    run_prepared_session_messages(
        &managed,
        "memory-agent",
        "thread-1",
        user("I really enjoy tea in the morning"),
    )
    .await
    .expect("Thread 1 Run");
    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);
    assert!(
        host.resources
            .memory_stores
            .fs()
            .get_by_path(&store, "/beverage-preference.md")
            .await
            .unwrap()
            .is_some(),
        "the preference should be saved in the bound store"
    );

    // Thread 2 (a fresh conversation): the saved memory is recalled into context
    // and the agent uses it to answer.
    let r = run_prepared_session_messages(
        &managed,
        "memory-agent",
        "thread-2",
        user("What beverage do I prefer?"),
    )
    .await
    .expect("Thread 2 Run");
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

#[tokio::test]
async fn claimed_child_terminal_memory_uses_parent_physical_commit_partition() {
    use awaken_dispatch::{DispatchQueue as _, WorkerResolver as _};

    // Causes: C1 a child Run is claimed for a Memory-enabled parent Session;
    // C2 the parent Runtime is cold or warm; C3 the Worker is co-located, so it
    // has the parent's authoritative commit and frozen Memory manifest; C4 the existing
    // recording Session application accepts the child's settlement. Effects: E1
    // one child terminal intent records child logical Thread/Run identity and
    // parent physical Session authority; E2 extraction writes one version to
    // the parent's selected Memory store. Constraints: cold recovery consumes
    // the previously frozen child dispatch, never a new Runtime-slot manifest;
    // a database-less Worker has no local Memory authority and uses the
    // Coordinator settlement observer instead. Decision table: R1=C1+cold+C3=>E1+E2;
    // R2=C1+warm+C3=>E1+E2; R3=C1+remote=>no local intent (the existing
    // `commit_ingest::claimed_child_commit_uses_guarded_parent_partition_and_logical_child_thread`
    // covers R3 at the Coordinator's single `worker_memory_settlement_observer`.
    for warm in [false, true] {
        let parent = format!("child-memory-parent-{warm}");
        let child = ThreadId(format!("child-memory-thread-{warm}"));
        let run = RunId(format!("child-memory-run-{warm}"));
        let host = memory_test_host(Arc::new(MemLoopModel), "memory-agent").await;
        let managed =
            managed_with_resource_source_using(host.clone(), install_recording_session_application);
        let memory_store = test_memory_store_id();
        let manifest =
            install_complete_memory_test_session(&managed, &parent, "memory-agent", &memory_store)
                .await;
        if warm {
            host.ctx_for(&parent, Some("memory-agent"))
                .await
                .expect("R2 warm parent context");
        } else {
            assert!(
                host.session_slots
                    .read_run(&parent, |run| run.runtime.is_none())
                    .unwrap_or(true),
                "R1 parent Runtime starts cold"
            );
        }
        let parent_commit = host
            .commit_for_read(&parent)
            .await
            .expect("parent commit authority");
        let activation = RunActivation::new(
            run.clone(),
            child.clone(),
            memory_test_snapshot("memory-agent"),
            vec![Message::text(
                MessageId(format!("child-memory-input-{warm}")),
                Role::User,
                "I really enjoy tea in the morning",
            )],
        );
        let dispatch = host.dispatch_store().expect("shared dispatch authority");
        let request = crate::agent_runner::child_dispatch_request(
            activation,
            ThreadId(parent.clone()),
            Some(manifest),
            host.agent.agent_publications.as_deref(),
        )
        .expect("freeze child dispatch through production owner")
        .with_session_activity_epoch(7);
        dispatch
            .enqueue(request)
            .await
            .expect("enqueue exact child");
        let claimed = dispatch
            .claim(
                host.dispatch_owner(),
                awaken_dispatch_execution::DEFAULT_LEASE_MS,
                &Default::default(),
            )
            .await
            .expect("claim child")
            .expect("child is runnable");
        assert!(
            host.dispatched_memory_terminal_observer(&claimed.request, parent_commit.clone())
                .await
                .expect("resolve exact child terminal observer")
                .is_some(),
            "C1/C3 frozen child Memory publication and parent binding produce an observer"
        );
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };
        let worker = resolver
            .worker_for_claimed(&claimed)
            .await
            .expect("resolve child through one factory");
        let (_, state) = worker
            .drive_claimed(claimed, host.clock())
            .await
            .expect("drive child")
            .expect("child settles");
        assert!(matches!(state, RunState::Ended(_)), "R1/R2 terminal child");
        assert!(
            parent_commit.run_state(&run).is_some(),
            "R1/R2 child commits through parent authority"
        );
        assert!(
            host.resources
                .memory
                .drain(std::time::Duration::from_secs(10))
                .await,
            "R1/R2 extraction drains"
        );
        let intent = host
            .resources
            .memory
            .extraction_repository()
            .get_extraction(&format!("memory-extraction:{}:{}", child.0, run.0))
            .await
            .expect("read exact child intent")
            .expect("R1/R2 child terminal extraction exists");
        assert_eq!(intent.session_id, parent, "R1/R2 physical partition");
        assert_eq!(intent.logical_thread_id(), child.0, "R1/R2 logical child");
        assert_eq!(intent.attempts, 1, "R1/R2 one extraction attempt");
        assert_eq!(
            intent.status,
            awaken_ext_memory::MemoryExtractionStatus::Completed,
            "R1/R2 terminal extraction completed"
        );
        assert_eq!(
            host.resources
                .memory_stores
                .fs()
                .list_versions(&memory_store)
                .await
                .expect("read governed Memory versions")
                .len(),
            1,
            "R1/R2 exactly one parent-bound Memory version"
        );
        assert!(
            !host
                .authority
                .durable_thread_exists(&child.0)
                .await
                .expect("child partition lookup"),
            "R1/R2 no child physical commit partition"
        );
    }
}

#[tokio::test]
async fn reopening_a_direct_terminal_thread_recovers_a_missing_extraction_outbox_intent() {
    use awaken_ext_memory::MemoryExtractionRepository as _;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-memory-outbox-{stamp}"));
    let thread = "memory-outbox-thread";
    let authority = Arc::new(crate::EphemeralRuntimeAuthority::new(system_clock()));

    // Cause/effect graph: C1 committed terminal truth exists; C2 its extraction
    // intent is absent; C3 the direct deployment materializes its canonical
    // DirectAttemptDriver; C4 the prior process left an unowned legacy sandbox
    // root. Effects: E1 the exact terminal observer creates and completes the
    // missing intent before any Environment effect; E2 ordinary Environment
    // recovery still rejects C4; E3 the completed extraction survives Host
    // replacement; E4 the original foreground delivery is the direct driver.
    // Decision table: D1=C1+C2+C3 => E1+E3+E4; D2=D1+C4 => E1+E2+E3+E4.
    // Constraint K1: neither rule adopts, deletes, or fabricates durable identity
    // for the legacy root. The test injects authority because runtime-host no
    // longer opens a commit Store.
    let (first, first_managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub")
            .await
            .with_store_dir(&dir)
            .await
            .with_runtime_authority(authority.clone())
            .expect("local authority is selected before Host construction"))
        .build()
        .expect("validated Host dependencies"),
    );
    first
        .run(None, thread, user("remember rust"))
        .await
        .expect("terminal run");
    assert!(
        first
            .session_slots
            .read_run(thread, |run| {
                run.runtime
                    .as_ref()
                    .is_some_and(|context| context.delivery.direct().is_some())
            })
            .unwrap_or(false),
        "D1/E4 foreground delivery must retain DirectAttemptDriver"
    );
    drop(first_managed);
    drop(first);

    // Rebind the frozen resource and reopen the committed thread. Context recovery
    // derives the missing outbox identity from the latest terminal run and inserts
    // the same durable intent normal after-commit delivery would have produced.
    let (second, second_managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub")
            .await
            .with_store_dir(&dir)
            .await
            .with_runtime_authority(authority)
            .expect("local authority is selected before Host construction"))
        .build()
        .expect("validated Host dependencies"),
    );
    bind_test_memory(&second, thread, "outbox-store", true);
    let commit = second
        .commit_for_read(thread)
        .await
        .expect("committed Thread reader");
    let thread_id = ThreadId(thread.into());
    let run = commit.latest_run(&thread_id).expect("terminal run record");
    let error = match second.ctx_for(thread, None).await {
        Ok(_) => panic!("D2/E2 legacy root must remain fail-closed"),
        Err(error) => error,
    };
    assert!(
        error.message.contains("legacy sandbox root") && error.message.contains("is not absent"),
        "D2/E2: {error:?}"
    );
    assert!(
        second
            .drain_runtime(std::time::Duration::from_secs(10))
            .await
    );

    let repository = awaken_session_store::SqliteManagedSessionRepository::open(
        &dir.join("sessions.db").to_string_lossy(),
    )
    .await
    .unwrap();
    let intent = repository
        .get_extraction(&format!("memory-extraction:{thread}:{}", run.id.0))
        .await
        .unwrap()
        .expect("recovered extraction intent");
    assert_eq!(
        intent.status,
        awaken_ext_memory::MemoryExtractionStatus::Completed
    );
    assert!(
        second
            .resources
            .memory_stores
            .fs()
            .get_by_path("outbox-store", "/user-prefs.md")
            .await
            .unwrap()
            .is_some(),
        "recovered outbox drives the same governed Memory store"
    );

    drop(second_managed);
    drop(second);
    std::fs::remove_dir_all(dir).ok();
}

/// An explicitly configured Awaken Memory extension selects only its frozen
/// Session binding; that same handle serves extraction + recall, while an
/// ordinary unbound Managed Session sees no store.
#[tokio::test]
async fn managed_memory_is_per_store_and_an_unbound_session_cannot_see_host_memory() {
    use awaken_session_configuration_contract::SessionInit;

    let snapshot = crate::config::server_config(
        "agent",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()],
        &std::collections::BTreeMap::from([(
            awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
            serde_json::json!({"binding_id": "test-input-0"}),
        )]),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .expect("literal model reference");
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("valid publication");
    let host = (crate::host::with_test_resource_validation(with_test_memory_mounter(
        crate::HostDependencies::ephemeral(Arc::new(MemLoopModel), "stub")
            .await
            .with_agent_publications(Arc::new(publications)),
    )))
    .build()
    .expect("validated Host dependencies");
    let unbound_store = test_memory_store_id();
    host.resources
        .memory_stores
        .fs()
        .create(&unbound_store, "/must-not-leak.md", "the user prefers tea")
        .await
        .unwrap();
    let store_a = test_memory_store_id();
    let store_b = test_memory_store_id();
    let managed = managed_with_resource_source(host.clone());
    let init = |agent: &str, store: Option<&str>| {
        SessionInit {
            workspace_id: host.local_workspace().into(),
            agent_id: agent.into(),
            delegate_ids: Vec::new(),
            tools: None,
            resource_revision: 0,
            resources: effective_resources(
                store
                    .map(|id| TestInput {
                        kind: "memory_store".into(),
                        id: id.into(),
                        mount_path: "/memory".into(),
                        // Workdir cannot OS-enforce read-only mounts, so this integration
                        // path uses a writable mount. The handle-level read-only
                        // invariant is covered separately.
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
        }
    };

    managed
        .install_complete_test_session("managed-write-a", init("agent", Some(&store_a)))
        .await
        .unwrap();
    run_prepared_session(
        &managed,
        "agent",
        "managed-write-a",
        vec![ContentBlock::text("I enjoy tea")],
    )
    .await
    .unwrap();
    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);
    assert!(
        host.resources
            .memory_stores
            .fs()
            .get_by_path(&store_a, "/beverage-preference.md")
            .await
            .unwrap()
            .is_some(),
        "extraction writes the bound platform store"
    );
    assert!(
        host.resources
            .memory_stores
            .fs()
            .list(&store_b, "/")
            .await
            .unwrap()
            .is_empty(),
        "a different store remains untouched"
    );

    let reply = |outcome: &awaken_session_application_contract::StepOutcome| {
        outcome
            .new_messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(|message| block_text(&message.content))
            .unwrap_or_default()
    };
    managed
        .install_complete_test_session("managed-read-a", init("agent", Some(&store_a)))
        .await
        .unwrap();
    let same = run_prepared_session(
        &managed,
        "agent",
        "managed-read-a",
        vec![ContentBlock::text("What do I prefer?")],
    )
    .await
    .unwrap();
    assert_eq!(reply(&same), "tea", "recall reads the same bound store");

    managed
        .install_complete_test_session("managed-read-b", init("agent", Some(&store_b)))
        .await
        .unwrap();
    let other = run_prepared_session(
        &managed,
        "agent",
        "managed-read-b",
        vec![ContentBlock::text("What do I prefer?")],
    )
    .await
    .unwrap();
    assert_eq!(reply(&other), "ok", "store B cannot recall store A");

    managed
        .install_complete_test_session("managed-unbound", init("unmanaged-agent", None))
        .await
        .unwrap();
    let unbound = run_prepared_session(
        &managed,
        "unmanaged-agent",
        "managed-unbound",
        vec![ContentBlock::text("What do I prefer?")],
    )
    .await
    .unwrap();
    assert_eq!(
        reply(&unbound),
        "ok",
        "a Session with no binding cannot see another governed store"
    );
}

/// Cause/effect decision table for a live Memory binding:
///
/// | requested manifest | relation to installed pin | effect |
/// |---|---|---|
/// | prepare blocked by lifecycle | absent | expose neither Environment nor manifest |
/// | durable active generation | absent beside an adopted live Environment | install as cold recovery |
/// | exact replay | equal | idempotent success; retain the live environment |
/// | original empty→Memory command, ACK lost | equal | preserve edited bytes and original heads |
/// | concurrent exact replays | equal | serialize on the existing Session lifecycle and converge |
/// | empty or any changed Memory field | mutation | reject before effects; retain bytes and manifest |
///
/// This is the active-active recovery case: a peer may ask the Runtime to replay
/// durable Session truth after the local projection has already been installed.
#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "requires Linux mount-namespace path fidelity"
)]
async fn exact_live_memory_manifest_replay_is_idempotent_but_change_fails_closed() {
    use awaken_session_configuration_contract::SessionInit;

    let sandbox_roots = tempfile::tempdir().unwrap();
    let host = (crate::host::with_test_resource_validation(with_test_memory_mounter(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_fallback_session_environment_provider(
                crate::session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
                    sandbox_roots.path().to_path_buf(),
                    false,
                    Arc::new(crate::session_environment::UnusedHandExecutorFactory),
                    "/bin/sh",
                    std::time::Duration::ZERO,
                )
                .into_session_provider(),
            ),
    ))).build().expect("validated Host dependencies");
    let store = test_memory_store_id();
    host.resources
        .memory_stores
        .fs()
        .create(&store, "/seed.md", "seed")
        .await
        .unwrap();
    let resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store,
        mount_path: "/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    let managed = managed_with_resource_source(host.clone());
    let init = SessionInit {
        workspace_id: host.local_workspace().into(),
        agent_id: "agent".into(),
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
    };
    let lifecycle_guard = host.session_slots.lock_lifecycle("memory-replay").await;
    let preparing = tokio::spawn({
        let managed = managed.clone();
        async move {
            managed
                .install_complete_test_session("memory-replay", init)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(
        host.realizer
            .session_environment("memory-replay")
            .await
            .is_none(),
        "preparation cannot expose Environment before the lifecycle owner commits the manifest"
    );
    drop(lifecycle_guard);
    preparing
        .await
        .expect("preparation task")
        .expect("prepare Session");
    run_prepared_session(
        &managed,
        "agent",
        "memory-replay",
        vec![ContentBlock::text("open")],
    )
    .await
    .unwrap();
    let environment = host
        .realizer
        .session_environment("memory-replay")
        .await
        .expect("live environment");
    let memory_file = sandbox_roots
        .path()
        .join("memory-replay/mnt/memory/seed.md");
    assert_eq!(std::fs::read(&memory_file).unwrap(), b"seed");
    std::fs::write(&memory_file, b"agent edit without newline").unwrap();
    let original_handle = environment.lifecycle().handle();

    // R-ACK: installed Memory is complete, but Control still carries the
    // original empty→Memory command. Both callers must succeed without a new
    // mount, byte rewind, head refresh, or physical Environment replacement.
    // This differs from a Memory→same-Memory no-op: its previous input is absent.
    let unacknowledged = resource_transition(
        host.local_workspace(),
        0,
        awaken_session_resource_contract::ResolvedSessionResources::default(),
        1,
        resources.clone(),
    );
    let (left, right) = tokio::join!(
        managed.apply_session_inputs("memory-replay", &unacknowledged),
        managed.apply_session_inputs("memory-replay", &unacknowledged),
    );
    left.expect("original command replay after local completion");
    right.expect("concurrent original command replay after local completion");
    assert_eq!(
        std::fs::read(&memory_file).unwrap(),
        b"agent edit without newline"
    );
    assert_eq!(environment.lifecycle().handle(), original_handle);

    // Reproduce the active-active recovery state observed in the black-box
    // pressure test: the durable sandbox binding is resident, while this process
    // has not yet projected the authoritative Resource generation.
    host.session_slots.update_realization("memory-replay", |r| {
        r.manifest = None;
        r.resources = Default::default();
        r.memory_bindings.clear();
        r.memory = None;
    });
    let replay = resource_transition(
        host.local_workspace(),
        0,
        resources.clone(),
        1,
        resources.clone(),
    );
    managed
        .environment()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "EnvironmentRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .apply_session_inputs("memory-replay", &replay)
        .await
        .expect("cold durable generation installs beside the adopted Environment");

    let (left, right) = tokio::join!(
        managed.apply_session_inputs("memory-replay", &replay),
        managed.apply_session_inputs("memory-replay", &replay),
    );
    left.expect("first exact durable replay");
    right.expect("concurrent exact durable replay");
    assert!(Arc::ptr_eq(
        &environment,
        &host
            .realizer
            .session_environment("memory-replay")
            .await
            .unwrap()
    ));

    let installed = host.realizer.thread_resource_manifest("memory-replay");
    for field in [
        "store",
        "config",
        "retention",
        "path",
        "access",
        "binding",
        "instructions",
        "created",
        "updated",
    ] {
        // R-MUT: vary each field independently; all other causes, including the
        // live Environment and completed pin, stay fixed. No denied attempt may
        // alter the installed projection, editable bytes or Copy baseline heads.
        let mut changed = resources.inputs()[0].clone();
        match field {
            "store" => {
                let awaken_session_resource_contract::ResolvedInputSource::MemoryStore {
                    memory_store_id,
                    config,
                } = &mut changed.source
                else {
                    unreachable!()
                };
                *memory_store_id = "other-store".into();
                config.memory_store_id = memory_store_id.clone();
            }
            "config" | "retention" => {
                let awaken_session_resource_contract::ResolvedInputSource::MemoryStore {
                    config,
                    ..
                } = &mut changed.source
                else {
                    unreachable!()
                };
                if field == "config" {
                    config.version = config.version.checked_next().unwrap();
                } else {
                    config.retention_policy.retention_days = Some(7);
                }
            }
            "path" => changed.mount_path = "/other-memory".into(),
            "access" => changed.access = ResourceAccess::ReadOnly,
            "binding" => changed.binding_id = "other-binding".into(),
            "instructions" => changed.instructions = Some("changed".into()),
            "created" => changed.created_at_unix_ms = Some(1),
            "updated" => changed.updated_at_unix_ms = Some(1),
            _ => unreachable!(),
        }
        let changed = awaken_session_resource_contract::ResolvedSessionResources::try_new(
            vec![changed],
            Vec::new(),
        )
        .unwrap();
        // Cross every mutation with previous=installed and previous=desired.
        // A no-op in the incoming command cannot override a different installed
        // Memory pin; compare the completed projection, not command equality.
        for previous in [resources.clone(), changed.clone()] {
            let error = managed
                .environment()
                .ok_or_else(|| {
                    awaken_session_types::RunError::unavailable(
                        "EnvironmentRuntime is unsupported by this runtime",
                    )
                })
                .expect("runtime role")
                .apply_session_inputs(
                    "memory-replay",
                    &resource_transition(host.local_workspace(), 1, previous, 2, changed.clone()),
                )
                .await
                .expect_err(field);
            assert!(
                error.message.contains("create-time only"),
                "{field}: {error:?}"
            );
        }
        assert_eq!(
            host.realizer.thread_resource_manifest("memory-replay"),
            installed,
            "{field}"
        );
        assert_eq!(environment.lifecycle().handle(), original_handle, "{field}");
        assert_eq!(
            std::fs::read(&memory_file).unwrap(),
            b"agent edit without newline",
            "{field}"
        );
    }
    let error = managed
        .environment()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "EnvironmentRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .apply_session_inputs(
            "memory-replay",
            &resource_transition(
                host.local_workspace(),
                1,
                resources.clone(),
                2,
                awaken_session_resource_contract::ResolvedSessionResources::default(),
            ),
        )
        .await
        .expect_err("live Memory removal must remain forbidden");
    assert!(error.message.contains("create-time only"));
}

#[tokio::test]
async fn published_agent_memory_config_can_disable_recall_and_extraction() {
    // Cause/effect rule: C1 a bound MemoryStore makes content available; C2 the
    // published parent Agent selects the `memory` plugin with both behavior flags
    // false. C1+C2 -> neither recall context nor terminal extraction (one Agent
    // publication is the authority; the resource carries no parallel policy).

    let snapshot = crate::config::server_config(
        "agent",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()],
        &std::collections::BTreeMap::from([(
            awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
            serde_json::json!({
                "binding_id": "test-input-0",
                "recall_enabled": false,
                "extraction_enabled": false
            }),
        )]),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .expect("literal model reference");
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("valid publication");
    let host = (crate::host::with_test_resource_validation(with_test_memory_mounter(
        crate::HostDependencies::ephemeral(Arc::new(MemLoopModel), "stub")
            .await
            .with_agent_publications(Arc::new(publications)),
    )))
    .build()
    .expect("validated Host dependencies");
    let store = test_memory_store_id();
    host.resources
        .memory_stores
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
        initial_branch: None,
        initial_commit: None,
    }]);
    managed
        .install_complete_test_session("managed-policy", init)
        .await
        .unwrap();
    let outcome = run_prepared_session(
        &managed,
        "agent",
        "managed-policy",
        vec![ContentBlock::text("What do I prefer?")],
    )
    .await
    .unwrap();
    let reply = outcome
        .new_messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| block_text(&message.content))
        .unwrap_or_default();
    assert_eq!(reply, "ok", "disabled recall does not inject store content");
    assert!(host.drain_runtime(std::time::Duration::from_secs(1)).await);
    assert!(
        host.resources
            .memory_stores
            .fs()
            .get_by_path(&store, "/beverage-preference.md")
            .await
            .unwrap()
            .is_none(),
        "disabled extraction does not mutate the store"
    );
}

/// The main agent awaits on a `write` (Ask-gated) then finishes on resume; the
/// extractor saves a memory. Proves resume-ended Runs trigger the aux agents.
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
        // from the main Run's `write` result that is also in its seeded context.
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
async fn resume_ended_run_triggers_memory_extraction() {
    // Decision rule: resume to Ended, drain background work, and require one
    // extraction over only the new committed messages.
    // Test design. Causes: C1 a resumed Run reaches Ended with new messages;
    // C2 memory extraction is enabled. Effects: E1 terminal resume schedules one
    // extraction over the new committed slice. Constraint/Invariant: extraction
    // follows committed terminal truth, not the caller's resume return. Decision
    // rule: resume to Ended, drain background work, and require one extraction.
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([memory_test_snapshot(
            "assistant",
        )])
        .expect("valid Memory Agent publication");
    let host = (crate::host::with_test_resource_validation(with_test_memory_mounter(
        crate::HostDependencies::ephemeral(Arc::new(ResumeMemModel), "stub")
            .await
            .with_gate_override(write_confirmation_gate())
            .with_agent_publications(Arc::new(publications)),
    )))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let store = test_memory_store_id();
    install_complete_memory_test_session(&managed, "t-res", "assistant", &store).await;

    // Run 1 awaits on the Ask-gated `write`.
    let r1 = run_prepared_session_messages(
        &managed,
        "assistant",
        "t-res",
        vec![Message::text(MessageId("u1".into()), Role::User, "hi")],
    )
    .await
    .expect("Run 1");
    assert!(
        matches!(r1.state(), RunState::Awaiting),
        "Run should await on write"
    );
    let pending = r1.pending().cloned().expect("a pending tool");

    // RunResume approves the write; the Run now ends and extraction fires.
    let r2 = host
        .resume(
            "t-res",
            &pending.tool_use_id,
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .expect("resume");
    assert!(
        matches!(r2.state, RunState::Ended(_)),
        "resume should end the Run"
    );

    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);
    let saved = host
        .resources
        .memory_stores
        .fs()
        .get_by_path(&store, "/resumed.md")
        .await
        .unwrap()
        .unwrap()
        .content
        .unwrap();
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

/// Cause/effect design: C1 Run 1 commits `alpha` and finishes memory extraction;
/// C2 Run 2 then commits `beta` on the same Thread. Effect E1: the advanced
/// extraction cursor supplies only `beta` to the second extraction, so `/seen.md`
/// excludes already-processed `alpha`. Decision rule X1=C1+C2=>E1.
#[tokio::test]
async fn extraction_cursor_only_processes_new_messages() {
    // Constraint/Invariant: the committed extraction cursor is the sole boundary;
    // previously processed messages must never re-enter a later seed. Decision rule:
    // advance the cursor once, append new messages, and require only the
    // suffix effect documented below.
    let host = memory_test_host(Arc::new(CursorModel), "memory-agent").await;
    let managed = managed_with_resource_source(host.clone());
    let store = test_memory_store_id();
    install_complete_memory_test_session(&managed, "t-cur", "memory-agent", &store).await;
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    run_prepared_session_messages(&managed, "memory-agent", "t-cur", user("alpha"))
        .await
        .expect("Run 1");
    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);
    run_prepared_session_messages(&managed, "memory-agent", "t-cur", user("beta"))
        .await
        .expect("Run 2");
    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);

    // The second extraction saw only "beta" — Run 1's "alpha" was past the cursor.
    let seen = host
        .resources
        .memory_stores
        .fs()
        .get_by_path(&store, "/seen.md")
        .await
        .unwrap()
        .unwrap()
        .content
        .unwrap();
    assert_eq!(
        seen, "beta",
        "cursor should exclude already-extracted messages"
    );
}

#[tokio::test]
async fn run_end_fires_background_memory_extraction() {
    // Test design. Causes: C1 an ordinary Run reaches Ended; C2 memory extraction
    // is configured. Effects: E1 terminal completion schedules extraction without
    // blocking the Run response; E2 draining background work persists the result.
    // Constraint/Invariant: committed Run end triggers exactly one background job.
    // Decision rule: end one Run, observe prompt return, then drain and require E2.
    let host = memory_test_host(Arc::new(MemoryHostModel), "memory-agent").await;
    let managed = managed_with_resource_source(host.clone());
    let store = test_memory_store_id();
    install_complete_memory_test_session(&managed, "t-mem", "memory-agent", &store).await;

    let input = vec![Message::text(
        MessageId("u1".into()),
        Role::User,
        "I really like rust",
    )];
    let result = run_prepared_session_messages(&managed, "memory-agent", "t-mem", input)
        .await
        .expect("Run");
    assert!(
        matches!(result.state(), RunState::Ended(_)),
        "Run should end"
    );

    let drained = host.drain_runtime(std::time::Duration::from_secs(10)).await;
    assert!(drained, "memory extraction should drain");

    let saved = host
        .resources
        .memory_stores
        .fs()
        .get_by_path(&store, "/user-prefs.md")
        .await
        .unwrap()
        .unwrap()
        .content
        .unwrap();
    assert_eq!(saved, "user likes rust");
}
