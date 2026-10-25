use super::*;
use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::run::Record;
use awaken_ext_memory::MemoryExtractorSnapshot;

#[derive(Clone)]
struct Committed {
    record: Option<Record>,
    messages: Vec<Message>,
}

impl CommittedThreadView for Committed {
    fn committed_messages(&self, _thread: &ThreadId) -> Vec<Message> {
        self.messages.clone()
    }
    fn run(&self, _run: &RunId) -> Option<Record> {
        self.record.clone()
    }
    fn latest_run(&self, _thread: &ThreadId) -> Option<Record> {
        self.record.clone()
    }
    fn resume_ticket(&self, _run: &RunId) -> Option<ResumeTicket> {
        None
    }
    fn open_wait_for_thread(&self, _thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        None
    }
}

fn intent() -> MemoryExtractionIntent {
    MemoryExtractionIntent::new_range(
        "extract",
        "terminal",
        "workspace",
        "session",
        "terminal",
        "store",
        1,
        0,
        1,
        vec![Message::text(
            MessageId("input".into()),
            Role::User,
            "Remember preferences",
        )],
        MemoryExtractorSnapshot::host_executor("extractor", "provider", "model", "native").unwrap(),
    )
    .unwrap()
}

fn committed(intent: &MemoryExtractionIntent, outputs: Vec<Message>) -> Committed {
    let mut messages = memory_extraction_input(intent);
    messages.extend(outputs);
    Committed {
        record: Some(Record {
            id: RunId(intent.auxiliary_run_id()),
            thread_id: ThreadId(intent.auxiliary_thread_id()),
            state: RunState::Ended(EndCause::NaturalEnd),
        }),
        messages,
    }
}

fn assistant(intent: &MemoryExtractionIntent, step: usize, content: Vec<ContentBlock>) -> Message {
    Message::new(
        MessageId::assistant(&RunId(intent.auxiliary_run_id()), step),
        Role::Assistant,
        content,
    )
}

fn write(
    intent: &MemoryExtractionIntent,
    step: usize,
    call: &str,
    name: &str,
    text: &str,
) -> Vec<Message> {
    let batch = format!("tool-batch:{}:{step}", intent.auxiliary_run_id());
    vec![
        assistant(
            intent,
            step,
            vec![ContentBlock::tool_use(
                call,
                "write_memory",
                serde_json::json!({"name":name,"content":text}),
            )],
        ),
        Message::new(
            MessageId::tool_result_for_batch(&batch, call),
            Role::Tool,
            vec![ContentBlock::tool_result(
                call,
                vec![ContentBlock::text("saved")],
            )],
        ),
    ]
}

#[test]
fn committed_agent_tool_calls_rebuild_only_policy_accepted_mutations() {
    // C1 an exact completed Run; C2 well-formed writes; C3 accepted/rejected content;
    // C4 unrelated tools. P1 C1+C2+accepted => normalized proposal; P2 rejected or
    // unrelated => none. The original policy assertions move with their owner.
    let i = intent();
    let mut outputs = write(
        &i,
        0,
        "valid",
        "User Pref",
        "The user prefers concise reviews.",
    );
    outputs.extend(write(
        &i,
        1,
        "rejected",
        "fix",
        "Fixed the bug by patching queue.rs.",
    ));
    outputs.push(assistant(
        &i,
        2,
        vec![ContentBlock::tool_use(
            "other",
            "unrelated",
            serde_json::json!({"name":"ignored","content":"ignored"}),
        )],
    ));
    assert_eq!(
        committed_memory_writes(&i, &committed(&i, outputs)).unwrap(),
        BTreeMap::from([(
            "/User-Pref.md".to_string(),
            "The user prefers concise reviews.".to_string()
        )])
    );
}

#[test]
fn extraction_proposals_require_exact_completed_run_and_input() {
    // Cause/effect table: C1 record present; C2 exact Run+Thread; C3 state;
    // C4 immutable input retained; C5 own assistant output. R1 !C1 or
    // Running/Indeterminate => same-Run uncertainty; R2 !C2 => refusal;
    // R3 known unsuccessful state => refusal; R4 !C4/!C5 => refusal;
    // R5 NaturalEnd/MaxSteps + C1,C2,C4,C5 => complete map (possibly empty).
    // Earlier conditions guard later ones; no failure reaches mutation planning.
    let i = intent();
    let valid = committed(
        &i,
        vec![assistant(&i, 0, vec![ContentBlock::text("No new memory")])],
    );
    for state in [
        RunState::Ended(EndCause::NaturalEnd),
        RunState::Ended(EndCause::MaxSteps),
    ] {
        let mut c = valid.clone();
        c.record.as_mut().unwrap().state = state;
        assert!(committed_memory_writes(&i, &c).unwrap().is_empty(), "R5");
    }
    let mut absent = valid.clone();
    absent.record = None;
    assert_eq!(
        committed_memory_writes(&i, &absent),
        Err(MemoryProposalError::ExecutionIndeterminate),
        "R1"
    );
    for state in [
        RunState::Running,
        RunState::Ended(EndCause::Indeterminate),
        RunState::Awaiting,
        RunState::Ended(EndCause::Cancelled),
        RunState::Ended(EndCause::Stopped("policy".into())),
    ] {
        let unknown = matches!(
            &state,
            RunState::Running | RunState::Ended(EndCause::Indeterminate)
        );
        let mut c = valid.clone();
        c.record.as_mut().unwrap().state = state;
        let error = committed_memory_writes(&i, &c).unwrap_err();
        assert_eq!(
            error,
            if unknown {
                MemoryProposalError::ExecutionIndeterminate
            } else {
                MemoryProposalError::UnsuccessfulRun
            }
        );
        match error.into_extraction_outcome(&i) {
            MemoryExtractionOutcome::ExecutionIndeterminate {
                auxiliary_run_id, ..
            } => {
                assert!(unknown);
                assert_eq!(auxiliary_run_id, i.auxiliary_run_id());
            }
            MemoryExtractionOutcome::PermanentFailure { .. } => assert!(!unknown),
            other => panic!("failure cannot yield mutations/retry: {other:?}"),
        }
    }
    for change_run in [false, true] {
        let mut c = valid.clone();
        let record = c.record.as_mut().unwrap();
        if change_run {
            record.id = RunId("foreign".into());
        } else {
            record.thread_id = ThreadId("foreign".into());
        }
        assert_eq!(
            committed_memory_writes(&i, &c),
            Err(MemoryProposalError::RunIdentityMismatch),
            "R2"
        );
    }
    for short in [false, true] {
        let mut c = valid.clone();
        if short {
            c.messages.clear();
        } else {
            c.messages[0].content = vec![ContentBlock::text("changed input")];
        }
        assert_eq!(
            committed_memory_writes(&i, &c),
            Err(MemoryProposalError::InputMismatch),
            "R4"
        );
    }
    assert_eq!(
        committed_memory_writes(&i, &committed(&i, Vec::new())),
        Err(MemoryProposalError::MissingOutput),
        "R4"
    );
    let mut foreign = valid;
    foreign.messages.last_mut().unwrap().id = MessageId("foreign-assistant-0".into());
    assert_eq!(
        committed_memory_writes(&i, &foreign),
        Err(MemoryProposalError::MissingOutput),
        "R4"
    );
}

#[test]
fn a_corrupt_write_never_returns_an_earlier_valid_partial_batch() {
    // C1 prior valid proposal; C2 malformed fields or duplicate same-Step call.
    // M1 C1+C2 => complete refusal, never Ok(partial). Exact WriteMemoryArgs
    // deserialization is shared with the live tool, including unknown-field denial.
    let i = intent();
    for bad in [
        serde_json::json!({"name":"bad"}),
        serde_json::json!({"content":"bad"}),
        serde_json::json!({"name":7,"content":"bad"}),
        serde_json::json!({"name":"bad","content":"bad","extra":true}),
    ] {
        let mut outputs = write(&i, 0, "valid", "pref", "likes tea");
        outputs.push(assistant(
            &i,
            1,
            vec![ContentBlock::tool_use("bad", "write_memory", bad)],
        ));
        assert_eq!(
            committed_memory_writes(&i, &committed(&i, outputs)),
            Err(MemoryProposalError::InvalidWrite)
        );
    }
    let mut outputs = write(&i, 0, "same", "pref", "likes tea");
    let repeated = outputs[0].content[0].clone();
    outputs[0].content.push(repeated);
    assert_eq!(
        committed_memory_writes(&i, &committed(&i, outputs)),
        Err(MemoryProposalError::InvalidWrite)
    );
}

#[test]
fn extraction_requires_one_successful_result_of_each_exact_tool_occurrence() {
    // Cause/effect graph: C1 exact Run/Step/call result; C2 success; C3 uniqueness.
    // T1 !C1 / !C2 / !C3 => no proposal map. T2 scoped results allow the same
    // provider call id in different Steps. T3 one unambiguous legacy result
    // remains readable; the canonical MessageId codec owns both formats.
    let i = intent();
    for rule in [
        "missing",
        "failed",
        "foreign-run",
        "foreign-step",
        "foreign-call",
        "duplicate",
        "wrong-role",
    ] {
        let mut outputs = write(&i, 0, "call", "pref", "likes tea");
        match rule {
            "missing" => {
                outputs.pop();
            }
            "failed" => {
                if let ContentBlock::ToolResult { is_error, .. } = &mut outputs[1].content[0] {
                    *is_error = true;
                }
            }
            "foreign-run" => {
                outputs[1].id = MessageId::tool_result_for_batch("tool-batch:foreign:0", "call")
            }
            "foreign-step" => {
                outputs[1].id = MessageId::tool_result_for_batch(
                    &format!("tool-batch:{}:1", i.auxiliary_run_id()),
                    "call",
                )
            }
            "foreign-call" => {
                outputs[1].id = MessageId::tool_result_for_batch(
                    &format!("tool-batch:{}:0", i.auxiliary_run_id()),
                    "other",
                )
            }
            "duplicate" => outputs.push(outputs[1].clone()),
            "wrong-role" => outputs[1].role = Role::User,
            _ => unreachable!(),
        }
        assert_eq!(
            committed_memory_writes(&i, &committed(&i, outputs)),
            Err(MemoryProposalError::UncommittedWrite),
            "{rule}"
        );
    }
    let mut outputs = write(&i, 0, "call", "pref", "likes tea");
    outputs.extend(write(&i, 1, "call", "pref", "likes coffee"));
    assert_eq!(
        committed_memory_writes(&i, &committed(&i, outputs)).unwrap()["/pref.md"],
        "likes coffee",
        "T2"
    );
    let mut legacy = write(&i, 0, "call", "pref", "likes tea");
    legacy[1].id = MessageId::tool_result("call");
    assert_eq!(
        committed_memory_writes(&i, &committed(&i, legacy)).unwrap()["/pref.md"],
        "likes tea",
        "T3"
    );
}

#[tokio::test]
async fn proposal_capture_has_no_content_or_recovery_state() {
    // C1 any number of tool proposals; C2 restart/new capture instance.
    // S1 C1+C2 => no entries or recovered map: committed Run output alone owns
    // the proposals. Both instances retain the same existing path normalization.
    let capture = MemoryWriteProposals;
    assert_eq!(
        capture.write("user prefs", "likes tea").await.unwrap(),
        "/user-prefs.md"
    );
    assert!(capture.entries().await.unwrap().is_empty());
    assert!(MemoryWriteProposals.entries().await.unwrap().is_empty());
}

#[test]
fn copied_seed_messages_cannot_become_extractor_outputs() {
    // C1 copied history has a write, even one shaped like this auxiliary Run;
    // C2 the auxiliary Run commits its own subsequent output. S2 only C2 is
    // decoded, so history replay cannot fabricate fresh durable Memory writes.
    let mut i = intent();
    i.transcript = write(&i, 0, "seed", "injected", "must not save");
    i.transcript_end = i.transcript.len();
    let mut outputs = write(&i, 1, "actual", "pref", "likes tea");
    outputs.push(assistant(&i, 2, vec![ContentBlock::text("done")]));
    let writes = committed_memory_writes(&i, &committed(&i, outputs)).unwrap();
    assert_eq!(
        writes,
        BTreeMap::from([("/pref.md".into(), "likes tea".into())])
    );
}

#[test]
fn memory_proposal_refusals_keep_uncertainty_distinct_from_corrupt_output() {
    // Complete finite decision table: E1 uncertain execution retains the same
    // auxiliary Run, E2 every other closed error is permanent. No error may
    // produce Mutations or Retryable, so corrupt output cannot be partially
    // applied or accidentally launch another extractor attempt.
    let i = intent();
    for error in [
        MemoryProposalError::ExecutionIndeterminate,
        MemoryProposalError::RunIdentityMismatch,
        MemoryProposalError::UnsuccessfulRun,
        MemoryProposalError::InputMismatch,
        MemoryProposalError::MissingOutput,
        MemoryProposalError::InvalidWrite,
        MemoryProposalError::UncommittedWrite,
    ] {
        match error.into_extraction_outcome(&i) {
            MemoryExtractionOutcome::ExecutionIndeterminate {
                auxiliary_run_id, ..
            } => {
                assert_eq!(error, MemoryProposalError::ExecutionIndeterminate);
                assert_eq!(auxiliary_run_id, i.auxiliary_run_id());
            }
            MemoryExtractionOutcome::PermanentFailure { .. } => {
                assert_ne!(error, MemoryProposalError::ExecutionIndeterminate)
            }
            other => panic!("a refusal cannot admit effects: {other:?}"),
        }
    }
}
