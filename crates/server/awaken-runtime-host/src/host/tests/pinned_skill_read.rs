//! Integration coverage of the existing Managed projection and lazy executor.
use super::*;
use awaken_runtime_contract::tool::{ToolCall, ToolError, ToolExecutor};

fn read_call(path: &str, range: Option<[i64; 2]>) -> ToolCall {
    ToolCall {
        call_id: "pinned-read".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({"file_path": path, "view_range": range}),
    }
}

#[derive(Default)]
struct PinnedReadModel(Mutex<Vec<ChatRequest>>);

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
        self.0.lock().unwrap().push(request);
        let output = match tool_results {
            0 => AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                call_id: "load-instructions".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"file_path": ".skills/diagnosis/SKILL.md"}),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                call_id: "load-reference".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"file_path": "/workspace/.skills/diagnosis/references/guide.md"}),
            }]),
            _ => AssistantOutput::text("您的产品主要卖给哪些客户？"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
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
    let host = Arc::new(
        SharedHost::new(model.clone(), "stub")
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path()),
    );
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
        host.session_environment("brain-first").await.is_none(),
        "P1/E3"
    );
    let requests = model.0.lock().unwrap();
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
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path()),
    );
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
    host.ctx_for("text-table", Some("assistant")).await.unwrap();
    let executor =
        crate::lazy_sandbox::DeferredSandboxExecutor::new(Arc::downgrade(&host), "text-table");
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
        assert!(host.session_environment("text-table").await.is_none());
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
        host.session_slots
            .read("text-table", |slot| slot.manifest.is_none())
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
    let make_host = || {
        Arc::new(
            SharedHost::new(Arc::new(OkModel), "stub")
                .with_skill_store(storage.path().join("skills"))
                .with_store_dir(storage.path())
                .with_local_workspace("workspace-pinned-read"),
        )
    };
    let original = make_host();
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
        .skills
        .append_version(original.local_workspace(), "pinned", v2)
        .await
        .unwrap()
        .unwrap();
    drop(managed);
    drop(original);
    let host = make_host();
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
    host.ctx_for("cold", Some("assistant")).await.unwrap();
    let executor = crate::lazy_sandbox::DeferredSandboxExecutor::new(Arc::downgrade(&host), "cold");
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
        host.session_slots.update("cold", |slot| {
            slot.skills = Some(vec![v1.clone()]);
            slot.workspace = Some(host.local_workspace().into());
            match mismatch {
                "version" => slot.skills.as_mut().unwrap()[0].version = 2,
                "hash" => slot.skills.as_mut().unwrap()[0].bundle_sha256 = "wrong".into(),
                _ => slot.workspace = Some("other-workspace".into()),
            }
        });
        assert!(
            executor
                .invoke(&read_call(".skills/pinned/SKILL.md", None))
                .await
                .is_err(),
            "P8 {mismatch}"
        );
        assert!(host.session_environment("cold").await.is_none());
    }
    host.session_slots.update("cold", |slot| {
        slot.skills = Some(vec![v1.clone()]);
        slot.workspace = Some(host.local_workspace().into());
        let binding = awaken_session_resource_contract::ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: "pinned".into(),
            version: 1,
            bundle_sha256: v1.bundle_sha256.clone(),
        };
        let resources = awaken_session_resource_contract::ResolvedSessionResources::default()
            .with_skills(vec![
                binding.clone(),
                awaken_session_resource_contract::ResolvedSkillBinding {
                    skill_id: "pinned!".into(),
                    ..binding
                },
            ])
            .unwrap();
        slot.resource_transition = Some(resource_transition(
            host.local_workspace(),
            0,
            Default::default(),
            1,
            resources,
        ));
    });
    assert!(
        executor
            .invoke(&read_call(".skills/pinned/SKILL.md", None))
            .await
            .is_err(),
        "P9"
    );
    assert!(host.session_environment("cold").await.is_none());
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
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path()),
    );
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
    host.ctx_for("install-retry", Some("assistant"))
        .await
        .unwrap();
    host.session_slots.update("install-retry", |slot| {
        slot.skills.as_mut().unwrap()[0].files[0].content.push(b'!')
    });
    let executor =
        crate::lazy_sandbox::DeferredSandboxExecutor::new(Arc::downgrade(&host), "install-retry");
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
    let environment = host.session_environment("install-retry").await.unwrap();
    let output = executor.invoke(&call).await.unwrap();
    assert!(
        output.text().contains("INSTALLED"),
        "P11 script installed before execution"
    );
    assert!(
        Arc::ptr_eq(
            &environment,
            &host.session_environment("install-retry").await.unwrap()
        ),
        "P11 one Environment"
    );
    assert!(
        host.session_slots
            .read("install-retry", |slot| slot.manifest.is_some())
            .unwrap(),
        "P11 physical completion"
    );
}
