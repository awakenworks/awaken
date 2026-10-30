//! Integration coverage of the existing Managed projection and lazy executor.
use super::*;
use awaken_tool_contract::tool::{ToolCall, ToolError, ToolExecutor};

fn read_call(path: &str, range: Option<[i64; 2]>) -> ToolCall {
    ToolCall {
        call_id: "pinned-read".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({"file_path": path, "view_range": range}),
    }
}

struct PinnedReadModel {
    requests: Mutex<Vec<ChatRequest>>,
    paths: Vec<&'static str>,
}

impl Default for PinnedReadModel {
    fn default() -> Self {
        Self {
            requests: Mutex::default(),
            paths: vec![
                ".skills/diagnosis/SKILL.md",
                "/workspace/.skills/diagnosis/references/guide.md",
            ],
        }
    }
}

#[async_trait::async_trait]
impl LlmExecutor for PinnedReadModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let tool_results = request
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .count();
        self.requests.lock().unwrap().push(request);
        let output = if let Some(path) = self.paths.get(tool_results) {
            AssistantOutput::from_tool_calls(vec![awaken_tool_contract::tool::ToolCall {
                call_id: format!("read-{tool_results}"),
                tool_id: "read".into(),
                arguments: serde_json::json!({"file_path": path}),
            }])
        } else {
            AssistantOutput::text("您的产品主要卖给哪些客户？")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// Gate the real binding boundary, delegating all authorization/receipt logic
/// to the existing fixture. Notifications are synchronization, not authority.
struct GatedSkillBindingSink {
    inner: BindingOrderSink,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    creates: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionEnvironmentBindingSink for GatedSkillBindingSink {
    async fn authorize(
        &self,
        intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_types::RunError,
    > {
        self.inner.authorize(intent).await
    }

    async fn persist(
        &self,
        receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_types::RunError>
    {
        if matches!(
            receipt.kind,
            awaken_session_contract::SessionEnvironmentEffectKind::Create
        ) {
            self.creates.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.persist(receipt).await
    }
}

#[tokio::test]
async fn pinned_reads_during_concurrent_first_hand_calls_do_not_join_sandbox_startup() {
    // Causes C1 two physical calls contend for first creation; C2 binding is
    // deliberately pending; C3 a pinned read arrives before publication.
    // B1 C1+C2+C3 => exact text, no physical completion, without releasing C2.
    // B2 release C2 => both scripts run against one durably created Environment;
    // subsequent physical reads equal the earlier immutable bytes. The gate
    // proves ordering; the timeout only detects a deadlock, not a latency SLO.
    let storage = tempfile::tempdir().unwrap();
    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path())
            .await,
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let version = frozen_skill_version(
        "race",
        "Race",
        "race",
        "RACE-BODY",
        &[("scripts/run.sh", "printf RACE-RUN")],
    );
    install_pinned_managed_skill_session(
        &host,
        &managed,
        "skill-race",
        "assistant",
        on_tool_use_environment(),
        &version,
        true,
    )
    .await
    .unwrap();
    host.run_assembly
        .ctx_for("skill-race", Some("assistant"))
        .await
        .unwrap();
    let sink = Arc::new(GatedSkillBindingSink {
        inner: BindingOrderSink {
            host: Arc::downgrade(&host),
            authorize_calls: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            observed_before_publish: AtomicBool::new(false),
            fail: false,
            require_realization: false,
            owned_session_id: Some("skill-race".into()),
            committed_environment: None,
        },
        entered: Default::default(),
        release: Default::default(),
        creates: AtomicUsize::new(0),
    });
    managed
        .run_assembly
        .install_test_environment_binding_sink(sink.clone());
    let executor = Arc::new(crate::lazy_sandbox::DeferredSandboxExecutor::new(
        Arc::downgrade(&host.run_assembly.installed_sessions),
        Arc::downgrade(&host.realization_executor),
        "skill-race",
    ));
    let bash = ToolCall {
        call_id: "race-hand".into(),
        tool_id: "bash".into(),
        arguments: serde_json::json!({"command": "sh .skills/race/scripts/run.sh"}),
    };
    let first = {
        let executor = executor.clone();
        let bash = bash.clone();
        tokio::spawn(async move { executor.invoke(&bash).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), sink.entered.notified())
        .await
        .unwrap();
    let second = {
        let executor = executor.clone();
        let bash = bash.clone();
        tokio::spawn(async move { executor.invoke(&bash).await })
    };
    let text = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.invoke(&read_call(".skills/race/SKILL.md", None)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        text.text().as_bytes(),
        version.skill_md().unwrap(),
        "B1 read while creation pending"
    );
    assert!(
        host.realization_executor
            .environments()
            .session_environment("skill-race")
            .await
            .expect("installed Sessions")
            .is_none()
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization("skill-race", |r| r.staging().manifest.is_none())
            .expect("installed Sessions")
            .unwrap()
    );
    assert!(!first.is_finished() && !second.is_finished());
    sink.release.notify_one();
    for result in [first.await.unwrap(), second.await.unwrap()] {
        assert!(
            result.unwrap().text().contains("RACE-RUN"),
            "B2 scripts dispatched after installation"
        );
    }
    assert_eq!(
        sink.creates.load(Ordering::SeqCst),
        1,
        "B2 one creation receipt"
    );
    assert_eq!(
        executor
            .invoke(&read_call(".skills/race/SKILL.md", None))
            .await
            .unwrap()
            .text(),
        text.text()
    );
}

#[tokio::test]
async fn cold_resource_replacement_and_revocation_never_return_old_skill_bytes() {
    // C1 no physical Environment; C2 exact v1 -> v2 -> empty transitions through
    // the Resource owner; C3 a previously advertised path remains in the model
    // context. B3 replacement => only v2 bytes; B4 revocation => typed failure;
    // both => no Sandbox, no latest fallback, no physical manifest promotion.
    let storage = tempfile::tempdir().unwrap();
    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path())
            .await,
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let v1 = frozen_skill_version("change", "Change", "change", "OLD-SECRET", &[]);
    install_pinned_managed_skill_session(
        &host,
        &managed,
        "skill-change",
        "assistant",
        on_tool_use_environment(),
        &v1,
        true,
    )
    .await
    .unwrap();
    host.run_assembly
        .ctx_for("skill-change", Some("assistant"))
        .await
        .unwrap();
    let executor = crate::lazy_sandbox::DeferredSandboxExecutor::new(
        Arc::downgrade(&host.run_assembly.installed_sessions),
        Arc::downgrade(&host.realization_executor),
        "skill-change",
    );
    assert!(
        executor
            .invoke(&read_call(".skills/change/SKILL.md", None))
            .await
            .unwrap()
            .text()
            .contains("OLD-SECRET")
    );
    let mut v2 = frozen_skill_version("change", "Change", "change", "NEW-SECRET", &[]);
    v2.version = 2;
    v2.id = "skver-change-2".into();
    host.capabilities
        .tools
        .skills
        .append_version(host.run_assembly.local_workspace(), "change", v2.clone())
        .await
        .unwrap()
        .unwrap();
    let resources = pinned_skill_resources(&v2);
    managed
        .realization_executor
        .apply_session_inputs(
            "skill-change",
            &resource_transition(
                host.run_assembly.local_workspace(),
                0,
                Default::default(),
                1,
                resources.clone(),
            ),
        )
        .await
        .unwrap();
    let result = executor
        .invoke(&read_call(".skills/change/SKILL.md", None))
        .await
        .unwrap();
    assert_eq!(
        result.text().as_bytes(),
        v2.skill_md().unwrap(),
        "B3 exact replacement"
    );
    managed
        .realization_executor
        .apply_session_inputs(
            "skill-change",
            &resource_transition(
                host.run_assembly.local_workspace(),
                1,
                resources,
                2,
                Default::default(),
            ),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            executor
                .invoke(&read_call(".skills/change/SKILL.md", None))
                .await,
            Err(ToolError::DefinitiveFailure(_))
        ),
        "B4 revoked bytes unavailable"
    );
    assert!(
        host.realization_executor
            .environments()
            .session_environment("skill-change")
            .await
            .expect("installed Sessions")
            .is_none()
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization("skill-change", |r| r.staging().manifest.is_none())
            .expect("installed Sessions")
            .unwrap()
    );
}

#[tokio::test]
async fn read_disabled_or_awaiting_permission_never_leaks_pinned_content() {
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };
    // C1 read disabled/enabled; C2 enabled read AlwaysAsk; C3 reply allow/deny;
    // C4 bash remains enabled. B5 !C1+C4 => descriptor absent and no body leak.
    // B6 C1+C2 => Awaiting, no content before reply. B7 allow => exact body;
    // B8 deny => error result without body. All rules => no Sandbox creation.
    // Calls traverse the real Runtime; invoking DeferredSandboxExecutor alone
    // would bypass the upstream policy and would not prove this requirement.
    for (enabled, allow) in [(false, false), (true, false), (true, true)] {
        let storage = tempfile::tempdir().unwrap();
        let model = Arc::new(PinnedReadModel {
            requests: Mutex::default(),
            paths: vec![".skills/diagnosis/SKILL.md"],
        });
        let host = (crate::host::with_test_resource_validation(
            crate::SessionExecutorBuilder::ephemeral(model.clone(), "stub")
                .await
                .with_skill_store(storage.path().join("skills"))
                .with_store_dir(storage.path())
                .await,
        ))
        .build()
        .expect("validated Host dependencies");
        let managed = managed_with_resource_source(host.clone());
        let version = frozen_skill_version(
            "diagnosis",
            "Diagnosis",
            "diagnosis",
            "PERMISSION-PROTECTED-BODY",
            &[],
        );
        let tools = awaken_session_configuration_contract::SessionToolConfiguration {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy::default(),
                overrides: vec![ToolPolicyOverride::new(
                    "read",
                    ToolExecutionPolicy {
                        enabled,
                        permission: ToolPermissionRequirement::AlwaysAsk,
                    },
                )],
            }],
            client_tools: vec![],
        };
        install_pinned_managed_skill_session_with_tools(
            &host,
            &managed,
            "skill-policy",
            "assistant",
            on_tool_use_environment(),
            &version,
            true,
            Some(tools),
        )
        .await
        .unwrap();
        let outcome = run_prepared_session_messages(
            &managed,
            "assistant",
            "skill-policy",
            user("read the Skill"),
        )
        .await
        .unwrap();
        {
            let requests = model.requests.lock().unwrap();
            assert!(requests[0].tools.iter().any(|tool| tool.id == "bash"));
            assert_eq!(
                requests[0].tools.iter().any(|tool| tool.id == "read"),
                enabled
            );
            assert!(
                !requests
                    .iter()
                    .flat_map(|request| &request.messages)
                    .any(|message| block_text(&message.content)
                        .contains("PERMISSION-PROTECTED-BODY")),
                "B5/B6 no body before permission"
            );
        }
        assert!(
            host.realization_executor
                .environments()
                .session_environment("skill-policy")
                .await
                .expect("installed Sessions")
                .is_none()
        );
        if enabled {
            assert!(matches!(outcome.state(), RunState::Awaiting), "B6");
            let pending = outcome.pending().unwrap();
            let decision = if allow {
                PermissionDecision::Allow { note: None }
            } else {
                PermissionDecision::Deny {
                    reason: Some("test denied".into()),
                }
            };
            let resumed = host
                .session_runs
                .resume(
                    "skill-policy",
                    &pending.tool_use_id,
                    HostResume::Permission(decision),
                )
                .await
                .unwrap();
            assert!(matches!(resumed.state, RunState::Ended(_)));
            let requests = model.requests.lock().unwrap();
            let leaked = requests
                .iter()
                .flat_map(|request| &request.messages)
                .filter(|message| message.role == Role::Tool)
                .any(|message| block_text(&message.content).contains("PERMISSION-PROTECTED-BODY"));
            assert_eq!(leaked, allow, "B7/B8 only allow discloses content");
        }
        assert!(
            host.realization_executor
                .environments()
                .session_environment("skill-policy")
                .await
                .expect("installed Sessions")
                .is_none()
        );
    }
}

#[tokio::test]
async fn failed_skill_file_write_blocks_tools_and_repaired_pin_reuses_environment() {
    // C1 valid UTF-8/hash bundle has a component exceeding this filesystem's
    // filename limit; C2 first physical call tries to install it; C3 retry has
    // the same pin or a corrected immutable v2. B9 C1+C2 => real file-write IO
    // failure, no tool side effect, no physical completion. B10 same pin =>
    // same failure/same Environment. B11 repaired pin => exact files installed,
    // script runs once, same Environment. This is actual OS IO, not a mocked
    // successful write or merely a cached hash mismatch. Repair changes the
    // authoritative pin; it never mutates bytes under an immutable v1 identity.
    let storage = tempfile::tempdir().unwrap();
    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path())
            .await,
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let bad_name = "a".repeat(4096);
    let v1 = frozen_skill_version(
        "write-fault",
        "Write fault",
        "fault",
        "instructions",
        &[
            ("scripts/run.sh", "printf EXECUTED"),
            (&bad_name, "cannot install"),
        ],
    );
    install_pinned_managed_skill_session(
        &host,
        &managed,
        "skill-write-fault",
        "assistant",
        on_tool_use_environment(),
        &v1,
        true,
    )
    .await
    .unwrap();
    host.run_assembly
        .ctx_for("skill-write-fault", Some("assistant"))
        .await
        .unwrap();
    let executor = crate::lazy_sandbox::DeferredSandboxExecutor::new(
        Arc::downgrade(&host.run_assembly.installed_sessions),
        Arc::downgrade(&host.realization_executor),
        "skill-write-fault",
    );
    let call = ToolCall {
        call_id: "after-install".into(),
        tool_id: "bash".into(),
        arguments: serde_json::json!({"command": "sh .skills/write-fault/scripts/run.sh > executed.txt"}),
    };
    for _ in 0..2 {
        let error = executor.invoke(&call).await.unwrap_err();
        assert!(
            error.to_string().contains("materialize_read_only_tree_at"),
            "B9/B10 file-write error: {error}"
        );
        assert!(
            !storage
                .path()
                .join("sandboxes/skill-write-fault/executed.txt")
                .exists(),
            "B9 no script effect"
        );
        assert!(
            host.run_assembly
                .installed_sessions
                .realizations()
                .read_realization("skill-write-fault", |r| r.staging().manifest.is_none())
                .expect("installed Sessions")
                .unwrap()
        );
    }
    let environment = host
        .realization_executor
        .environments()
        .session_environment("skill-write-fault")
        .await
        .expect("installed Sessions")
        .unwrap();
    let mut v2 = frozen_skill_version(
        "write-fault",
        "Write fault",
        "fault",
        "repaired instructions",
        &[("scripts/run.sh", "printf EXECUTED")],
    );
    v2.version = 2;
    v2.id = "skver-write-fault-2".into();
    host.capabilities
        .tools
        .skills
        .append_version(
            host.run_assembly.local_workspace(),
            "write-fault",
            v2.clone(),
        )
        .await
        .unwrap()
        .unwrap();
    managed
        .realization_executor
        .apply_session_inputs(
            "skill-write-fault",
            &resource_transition(
                host.run_assembly.local_workspace(),
                0,
                Default::default(),
                1,
                pinned_skill_resources(&v2),
            ),
        )
        .await
        .unwrap();
    executor.invoke(&call).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(
            storage
                .path()
                .join("sandboxes/skill-write-fault/executed.txt")
        )
        .unwrap(),
        "EXECUTED",
        "B11 exact script side effect"
    );
    assert!(Arc::ptr_eq(
        &environment,
        &host
            .realization_executor
            .environments()
            .session_environment("skill-write-fault")
            .await
            .expect("installed Sessions")
            .unwrap()
    ));
    assert_eq!(
        executor
            .invoke(&read_call(".skills/write-fault/SKILL.md", None))
            .await
            .unwrap()
            .text()
            .as_bytes(),
        v2.skill_md().unwrap()
    );
}

#[tokio::test]
async fn managed_on_tool_use_reads_instruction_and_reference_before_reply_without_sandbox() {
    // Causes C1 exact Managed pin, C2 OnToolUse, C3 script/reference support files,
    // C4 Runtime invokes read then replies. Effects E1 metadata only in initial
    // prompt, E2 exact instructions/reference arrive through normal Tool results,
    // E3 reply completes without Sandbox. Rule P1 C1+C2+C3+C4 => E1+E2+E3.
    // Constraint: no direct invocation bypass; the real Runtime target routing,
    // tool admission, message history and executor all participate.
    let storage = tempfile::tempdir().unwrap();
    let model = Arc::new(PinnedReadModel::default());
    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(model.clone(), "stub")
            .await
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path())
            .await,
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let version = frozen_skill_version(
        "diagnosis",
        "Diagnosis",
        "Diagnose招商",
        "Read references/guide.md before asking.",
        &[
            ("references/guide.md", "根据已知资料逐步提问。"),
            ("scripts/report.py", "print('report')"),
        ],
    );
    install_pinned_managed_skill_session(
        &host,
        &managed,
        "brain-first",
        "assistant",
        on_tool_use_environment(),
        &version,
        true,
    )
    .await
    .unwrap();
    run_prepared_session_messages(
        &managed,
        "assistant",
        "brain-first",
        vec![Message::text(
            MessageId("customer".into()),
            Role::User,
            "我做家具，想招经销商",
        )],
    )
    .await
    .unwrap();
    assert!(
        host.realization_executor
            .environments()
            .session_environment("brain-first")
            .await
            .expect("installed Sessions")
            .is_none(),
        "P1/E3"
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "P1 two reads then reply");
    let system = requests[0]
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .map(|message| block_text(&message.content))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(system.contains(".skills/diagnosis/SKILL.md"));
    assert!(
        !system.contains("Read references/guide.md before asking."),
        "P1/E1 progressive disclosure"
    );
    let results = requests[2]
        .messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| block_text(&message.content))
        .collect::<Vec<_>>();
    assert!(
        results
            .iter()
            .any(|result| result.as_bytes() == version.skill_md().unwrap()),
        "P1/E2 exact full instruction bytes"
    );
    assert!(
        results
            .iter()
            .any(|result| result == "根据已知资料逐步提问。"),
        "P1/E2 reference"
    );
}

#[tokio::test]
async fn pinned_read_path_and_text_decision_table() {
    // Causes: C1 path is advertised bound namespace; C2 exact file exists;
    // C3 bytes UTF-8 and within default limit; C4 range; C5 args valid.
    // Rules: P2 C1+C2+C3+C5 full/relative/absolute => exact text;
    // P3 same+range => requested lines; P4 !C1 (unknown stem/traversal) or
    // !C2 => definitive failure, no fallback; P5 !C3 => definitive failure;
    // P6 !C5 => InvalidArguments. Every row => no Sandbox and no physical
    // manifest promotion. Plain workspace reads are covered by L3/L11.
    let storage = tempfile::tempdir().unwrap();
    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path())
            .await,
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let mut version = frozen_skill_version(
        "files",
        "Files",
        "files",
        "instructions",
        &[
            ("references/guide.md", "一\r\n二\n三\n"),
            ("scripts/run.sh", "#!/bin/sh\necho ok"),
        ],
    );
    version.files.extend([
        awaken_skill_store::SkillBundleFile {
            path: "binary.dat".into(),
            content: vec![0xff],
            executable: false,
        },
        awaken_skill_store::SkillBundleFile {
            path: "large.txt".into(),
            content: vec![b'a'; 256 * 1024 + 1],
            executable: false,
        },
    ]);
    version.bundle_sha256 = awaken_skill_store::bundle_sha256(&version.files);
    install_pinned_managed_skill_session(
        &host,
        &managed,
        "text-table",
        "assistant",
        on_tool_use_environment(),
        &version,
        true,
    )
    .await
    .unwrap();
    host.run_assembly
        .ctx_for("text-table", Some("assistant"))
        .await
        .unwrap();
    let executor = crate::lazy_sandbox::DeferredSandboxExecutor::new(
        Arc::downgrade(&host.run_assembly.installed_sessions),
        Arc::downgrade(&host.realization_executor),
        "text-table",
    );
    for (path, range, expected) in [
        (
            ".skills/files/references/guide.md",
            None,
            Some("一\r\n二\n三\n"),
        ),
        (
            "/workspace/.skills/files/references/guide.md",
            Some([2, 2]),
            Some("二"),
        ),
        (
            "./.skills/files/references/guide.md",
            Some([i64::MIN, 1]),
            Some("一\r"),
        ),
        (
            ".skills/files/references/guide.md",
            Some([2, -1]),
            Some("二\n三\n"),
        ),
        (".skills/files/references/guide.md", Some([9, 2]), Some("")),
        (
            ".skills/files/scripts/run.sh",
            None,
            Some("#!/bin/sh\necho ok"),
        ),
        (".skills/unknown/SKILL.md", None, None),
        (".skills/files/missing.md", None, None),
        (".skills/files/../../private.md", None, None),
        (".skills/files/binary.dat", None, None),
        (".skills/files/large.txt", Some([1, 1]), None),
    ] {
        let result = executor.invoke(&read_call(path, range)).await;
        match expected {
            Some(expected) => assert_eq!(result.unwrap().text(), expected, "P2/P3 {path}"),
            None => assert!(
                matches!(result, Err(ToolError::DefinitiveFailure(_))),
                "P4/P5 {path}: {result:?}"
            ),
        }
        assert!(
            host.realization_executor
                .environments()
                .session_environment("text-table")
                .await
                .expect("installed Sessions")
                .is_none()
        );
    }
    for arguments in [
        serde_json::json!({"file_path": ".skills/files/SKILL.md", "extra": true}),
        serde_json::json!({"file_path": 4}),
    ] {
        let result = executor
            .invoke(&ToolCall {
                arguments,
                ..read_call("", None)
            })
            .await;
        assert!(
            matches!(result, Err(ToolError::InvalidArguments(_))),
            "P6 {result:?}"
        );
    }
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization("text-table", |r| r.staging().manifest.is_none())
            .expect("installed Sessions")
            .unwrap(),
        "no physical completion invented"
    );
}

#[tokio::test]
async fn cold_skill_read_keeps_pin_after_catalog_update_and_rejects_stale_projection() {
    // Causes: C1 store contains v1 and newer v2; C2 cold Worker resolves v1
    // binding; C3 cached bytes match desired identity; C4 workspace matches.
    // P7 C1+C2+C3+C4 => v1, no ambient latest fallback or Sandbox;
    // P8 !C3 (version/hash changed) or !C4 => fail, no bytes/no Sandbox;
    // P9 two exact bindings share a projected stem => fail, never choose first.
    // Missing/corrupt Store bytes at admission use existing S2 registry tests.
    let storage = tempfile::tempdir().unwrap();
    let make_host = || async {
        (crate::host::with_test_resource_validation(
            crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
                .await
                .with_skill_store(storage.path().join("skills"))
                .with_store_dir(storage.path())
                .await
                .with_local_workspace("workspace-pinned-read"),
        ))
        .build()
        .expect("validated Host dependencies")
    };
    let original = make_host().await;
    let managed = managed_with_resource_source(original.clone());
    let v1 = frozen_skill_version("pinned", "Pinned", "pinned", "VERSION ONE", &[]);
    install_pinned_managed_skill_session(
        &original,
        &managed,
        "original",
        "assistant",
        on_tool_use_environment(),
        &v1,
        true,
    )
    .await
    .unwrap();
    let mut v2 = frozen_skill_version("pinned", "Pinned", "pinned", "VERSION TWO", &[]);
    v2.version = 2;
    v2.id = "skver-pinned-2".into();
    original
        .capabilities
        .tools
        .skills
        .append_version(original.local_workspace(), "pinned", v2)
        .await
        .unwrap()
        .unwrap();
    drop(managed);
    drop(original);
    let host = make_host().await;
    let managed = managed_with_resource_source(host.clone());
    install_pinned_managed_skill_session(
        &host,
        &managed,
        "cold",
        "assistant",
        on_tool_use_environment(),
        &v1,
        false,
    )
    .await
    .unwrap();
    host.run_assembly
        .ctx_for("cold", Some("assistant"))
        .await
        .unwrap();
    let executor = crate::lazy_sandbox::DeferredSandboxExecutor::new(
        Arc::downgrade(&host.run_assembly.installed_sessions),
        Arc::downgrade(&host.realization_executor),
        "cold",
    );
    assert_eq!(
        executor
            .invoke(&read_call(".skills/pinned/SKILL.md", None))
            .await
            .unwrap()
            .text()
            .as_bytes(),
        v1.skill_md().unwrap(),
        "P7"
    );
    for mismatch in ["version", "hash", "workspace"] {
        host.run_assembly
            .installed_sessions
            .install_with("cold", |installed| {
                installed.realization.staging_mut().skills = Some(vec![v1.clone()]);
                installed.replace_frozen_with(|next| {
                    next.workspace = Some(host.run_assembly.local_workspace().into())
                });
                match mismatch {
                    "version" => {
                        installed.realization.staging_mut().skills.as_mut().unwrap()[0].version = 2
                    }
                    "hash" => {
                        installed.realization.staging_mut().skills.as_mut().unwrap()[0]
                            .bundle_sha256 = "wrong".into()
                    }
                    _ => installed.replace_frozen_with(|next| {
                        next.workspace = Some("other-workspace".into())
                    }),
                }
            })
            .expect("installed Sessions");
        assert!(
            executor
                .invoke(&read_call(".skills/pinned/SKILL.md", None))
                .await
                .is_err(),
            "P8 {mismatch}"
        );
        assert!(
            host.realization_executor
                .environments()
                .session_environment("cold")
                .await
                .expect("installed Sessions")
                .is_none()
        );
    }
    host.run_assembly
        .installed_sessions
        .install_with("cold", |installed| {
            installed.realization.staging_mut().skills = Some(vec![v1.clone()]);
            installed.replace_frozen_with(|next| {
                next.workspace = Some(host.run_assembly.local_workspace().into())
            });
            let binding = crate::session_resource::ResolvedSkillBinding {
                kind: awaken_agent_contract::AgentSkillKind::Custom,
                skill_id: "pinned".into(),
                version: 1,
                bundle_sha256: v1.bundle_sha256.clone(),
            };
            let resources = crate::session_resource::ResolvedSessionResources::default()
                .with_skills(vec![
                    binding.clone(),
                    crate::session_resource::ResolvedSkillBinding {
                        skill_id: "pinned!".into(),
                        ..binding
                    },
                ])
                .unwrap();
            installed.realization.staging_mut().transition = Some(resource_transition(
                host.run_assembly.local_workspace(),
                0,
                Default::default(),
                1,
                resources,
            ));
        })
        .expect("installed Sessions");
    assert!(
        executor
            .invoke(&read_call(".skills/pinned/SKILL.md", None))
            .await
            .is_err(),
        "P9"
    );
    assert!(
        host.realization_executor
            .environments()
            .session_environment("cold")
            .await
            .expect("installed Sessions")
            .is_none()
    );
}

#[tokio::test]
async fn lazy_skill_installation_reloads_exact_bytes_and_reuses_environment() {
    // Causes C1 first physical tool on a pinned lazy Session; C2 a stale/corrupt
    // process-local projection exists; C3 the canonical immutable source is
    // available; C4 a second physical call follows. P10 C1+C2+C3 => reload exact
    // bytes and install the original script before dispatch; P11 C4 => reuse
    // the same bound Environment, with no second creation. The process-local
    // projection is not authority and cannot replace persisted pinned bytes.
    let storage = tempfile::tempdir().unwrap();
    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path())
            .await,
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let version = frozen_skill_version(
        "retry",
        "Retry",
        "retry",
        "instructions",
        &[("scripts/run.sh", "printf INSTALLED")],
    );
    install_pinned_managed_skill_session(
        &host,
        &managed,
        "install-retry",
        "assistant",
        on_tool_use_environment(),
        &version,
        true,
    )
    .await
    .unwrap();
    host.run_assembly
        .ctx_for("install-retry", Some("assistant"))
        .await
        .unwrap();
    host.run_assembly
        .installed_sessions
        .realizations()
        .update_realization("install-retry", |r| {
            r.staging_mut().skills.as_mut().unwrap()[0].files[0]
                .content
                .push(b'!')
        })
        .expect("installed Sessions");
    let executor = crate::lazy_sandbox::DeferredSandboxExecutor::new(
        Arc::downgrade(&host.run_assembly.installed_sessions),
        Arc::downgrade(&host.realization_executor),
        "install-retry",
    );
    let call = ToolCall {
        call_id: "run-after-install".into(),
        tool_id: "bash".into(),
        arguments: serde_json::json!({"command": "sh .skills/retry/scripts/run.sh"}),
    };
    let first = executor.invoke(&call).await.unwrap();
    assert!(
        first.text().contains("INSTALLED"),
        "P10 exact script installed before dispatch"
    );
    let original = executor
        .invoke(&read_call(".skills/retry/SKILL.md", None))
        .await
        .unwrap();
    assert_eq!(
        original.text().as_bytes(),
        version.skill_md().unwrap(),
        "P10 canonical bytes replace stale projection"
    );
    let environment = host
        .realization_executor
        .environments()
        .session_environment("install-retry")
        .await
        .expect("installed Sessions")
        .unwrap();
    let output = executor.invoke(&call).await.unwrap();
    assert!(
        output.text().contains("INSTALLED"),
        "P11 script installed before execution"
    );
    assert!(
        Arc::ptr_eq(
            &environment,
            &host
                .realization_executor
                .environments()
                .session_environment("install-retry")
                .await
                .expect("installed Sessions")
                .unwrap()
        ),
        "P11 one Environment"
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization("install-retry", |r| r.staging().manifest.is_some())
            .expect("installed Sessions")
            .unwrap(),
        "P11 physical completion"
    );
}
