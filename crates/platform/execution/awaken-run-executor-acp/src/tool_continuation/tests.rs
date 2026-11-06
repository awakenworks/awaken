// Receipt-projection behavior lives beside the continuation it verifies.
use super::*;

#[tokio::test]
async fn owned_receipt_projection_reuses_committed_facts_across_runs_without_sdk_acknowledgements()
{
    // R1 Runtime-minted result + Assistant ToolUse + owned tool => include exact
    // success/error, original committed input and data even with no SDK call
    // history. R2 SDK-minted result => exclude (already SDK history).
    // R3 unowned tool, unpaired result or User-authored ToolUse => exclude.
    // R4 later Run / repeated projection => same ordered receipts, zero commits;
    // R5 two valid Assistant occurrences reuse one provider call id => each
    // batch-scoped result pairs with its own tool, while legacy ids still read.
    // R6 malformed, foreign-Run or wrong-Step batch coordinates => no
    // Runtime-owned receipt, even when a provider call id matches.
    // R7 parallel ACP calls occupy separate Assistant notifications, followed
    // by an Assistant text update before their Runtime results. Both exact
    // results must survive; a later notification cannot erase another pending
    // call. Results settle only their own id; same-id reuse remains R5.
    // A finalized ToolBatch publishes every result before the next Assistant
    // inference; a valid old result cannot first appear after the next use.
    // loss before external delivery cannot erase a previously committed result.
    // Context is a read-only projection, never a fabricated SDK ToolResult or
    // durable prompt copy. No tool-result text is interpreted as instructions.
    let committed = Arc::new(RecordingCoordinator::default());
    let probe = OwnedContinuationProbe {
        resumes: Mutex::new(Vec::new()),
        recoveries: AtomicUsize::new(0),
        fail: false,
    };
    let mut activation = activation_without_session_home();
    let mut messages = Vec::new();
    for (call, tool, role, sdk, is_error) in [
        ("ok", "write", Role::Assistant, false, false),
        ("failed", "write", Role::Assistant, false, true),
        ("sdk", "write", Role::Assistant, true, false),
        (
            "external",
            "mcp__external__write",
            Role::Assistant,
            false,
            false,
        ),
        ("user", "write", Role::User, false, false),
    ] {
        messages.push(Message::new(
            MessageId(format!("use-{call}")),
            role,
            vec![ContentBlock::tool_use(
                call,
                tool,
                serde_json::json!({"file_path":format!("{call}.txt")}),
            )],
        ));
        messages.push(Message::new(
            if sdk {
                MessageId(format!("sdk-result-{call}"))
            } else {
                MessageId::tool_result(call)
            },
            Role::Tool,
            vec![ContentBlock::tool_result_with_error(
                call,
                vec![ContentBlock::text(format!(
                    "receipt {call}: é中文🙂\nquoted tool data"
                ))],
                is_error,
            )],
        ));
    }
    messages.push(Message::new(
        MessageId::tool_result("unpaired"),
        Role::Tool,
        vec![ContentBlock::tool_result(
            "unpaired",
            vec![ContentBlock::text("unpaired data")],
        )],
    ));
    messages.push(Message::new(
        MessageId::assistant(&RunId("forged-run".into()), 0),
        Role::Assistant,
        vec![ContentBlock::tool_use(
            "forged",
            "write",
            serde_json::json!({}),
        )],
    ));
    for batch in [
        "not-a-batch",
        "tool-batch:other-run:0",
        "tool-batch:forged-run:1",
    ] {
        messages.push(Message::new(
            MessageId::tool_result_for_batch(batch, "forged"),
            Role::Tool,
            vec![ContentBlock::tool_result(
                "forged",
                vec![ContentBlock::text("must not enter ACP context")],
            )],
        ));
    }
    let parallel = RunId("parallel".into());
    for (step, call, tool) in [
        (0, "parallel-a", "write"),
        (1, "parallel-b", "desktop_search"),
    ] {
        messages.push(Message::new(
            MessageId::assistant(&parallel, step),
            Role::Assistant,
            vec![ContentBlock::tool_use(call, tool, serde_json::json!({}))],
        ));
    }
    messages.push(Message::text(
        MessageId::assistant(&parallel, 2),
        Role::Assistant,
        "progress",
    ));
    for (step, call) in [(0, "parallel-a"), (1, "parallel-b")] {
        messages.push(Message::new(
            MessageId::tool_result_for_batch(&format!("tool-batch:parallel:{step}"), call),
            Role::Tool,
            vec![ContentBlock::tool_result(
                call,
                vec![ContentBlock::text(call)],
            )],
        ));
    }
    for (run, tool, text) in [
        ("repeat-first", "write", "first owned result"),
        ("repeat-second", "desktop_search", "second owned result"),
    ] {
        messages.push(Message::new(
            MessageId::assistant(&RunId(run.into()), 0),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "same-provider-id",
                tool,
                serde_json::json!({"occurrence":run}),
            )],
        ));
        messages.push(Message::new(
            MessageId::tool_result_for_batch(&format!("tool-batch:{run}:0"), "same-provider-id"),
            Role::Tool,
            vec![ContentBlock::tool_result(
                "same-provider-id",
                vec![ContentBlock::text(text)],
            )],
        ));
    }
    committed
        .commit(ThreadCommit::assemble(
            activation.thread_id.clone(),
            RunDisposition::running(activation.run_id.clone()),
            true,
            messages,
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
    let context = RuntimeRunContext::new()
        .with_reader(committed.clone())
        .with_commit(committed.clone());
    let count = committed.commits.lock().unwrap().len();
    let original =
        crate::tool_continuation::owned_tool_result_context(&probe, &activation, &context)
            .unwrap()
            .unwrap()
            .text_content();
    let (_, json) = original.split_once('\n').unwrap();
    let receipts: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
    assert_eq!(receipts.len(), 6);
    assert_eq!(receipts[0]["call_id"], "ok");
    assert_eq!(receipts[0]["is_error"], false);
    assert_eq!(
        receipts[0]["input"],
        serde_json::json!({"file_path":"ok.txt"})
    );
    assert_eq!(
        receipts[1]["input"],
        serde_json::json!({"file_path":"failed.txt"})
    );
    assert_eq!(receipts[1]["call_id"], "failed");
    assert_eq!(receipts[1]["is_error"], true);
    assert_eq!(
        receipts[1]["content"][0]["text"],
        "receipt failed: é中文🙂\nquoted tool data"
    );
    assert_eq!(receipts[2]["call_id"], "parallel-a", "R7");
    assert_eq!(receipts[3]["call_id"], "parallel-b", "R7");
    assert_eq!(receipts[4]["tool"], "write", "R5 exact first occurrence");
    assert_eq!(receipts[4]["content"][0]["text"], "first owned result");
    assert_eq!(
        receipts[4]["input"]["occurrence"], "repeat-first",
        "R5 original input"
    );
    assert_eq!(
        receipts[5]["input"]["occurrence"], "repeat-second",
        "R5 never the latest reused-id input"
    );
    assert_eq!(
        receipts[5]["tool"], "desktop_search",
        "R5 exact second occurrence"
    );
    assert_eq!(receipts[5]["content"][0]["text"], "second owned result");
    activation.run_id = RunId("later-run".into());
    for _ in 0..2 {
        let projected =
            crate::tool_continuation::owned_tool_result_context(&probe, &activation, &context)
                .unwrap()
                .unwrap();
        assert_eq!(projected.text_content(), original);
        assert_eq!(projected.role, Role::System);
    }
    assert_eq!(committed.commits.lock().unwrap().len(), count);
    assert!(probe.resumes.lock().unwrap().is_empty());
    assert_eq!(probe.recoveries.load(Ordering::SeqCst), 0);

    // R8 an oversized aggregate over real committed receipts -> typed refusal,
    // same complete history, no recovery/effect or additional commit. The limit
    // must not silently truncate a result or reissue its already settled call.
    let run = RunId("large".into());
    committed
        .commit(ThreadCommit::assemble(
            activation.thread_id.clone(),
            RunDisposition::running(activation.run_id.clone()),
            true,
            vec![
                Message::new(
                    MessageId::assistant(&run, 0),
                    Role::Assistant,
                    vec![ContentBlock::tool_use(
                        "large-call",
                        "write",
                        serde_json::json!({}),
                    )],
                ),
                Message::new(
                    MessageId::tool_result_for_batch("tool-batch:large:0", "large-call"),
                    Role::Tool,
                    vec![ContentBlock::tool_result(
                        "large-call",
                        vec![ContentBlock::text("中".repeat(400_000))],
                    )],
                ),
            ],
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
    let count = committed.commits.lock().unwrap().len();
    let error = crate::tool_continuation::owned_tool_result_context(&probe, &activation, &context)
        .unwrap_err();
    assert_eq!(error.code(), "acp_receipt_context_exceeded");
    assert_eq!(committed.commits.lock().unwrap().len(), count);
    assert!(probe.resumes.lock().unwrap().is_empty());
    assert_eq!(probe.recoveries.load(Ordering::SeqCst), 0);
}
