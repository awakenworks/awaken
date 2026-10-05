use super::*;
use awaken_agent_contract::agent::run::Failure;
use awaken_runtime_contract::tool::{
    RawTool, ToolConcurrency, ToolError, ToolExecutionTarget, ToolExecutor, ToolOutput,
};
use awaken_runtime_contract::tool_batch::{
    ActiveToolBatch, ToolBatch, ToolBatchPhase, ToolExecutionDomain,
};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug)]
enum Preparation {
    Ready,
    Unavailable,
    Failed,
    Indeterminate,
    Invalidated,
}

struct SandboxDeclaration(&'static str);

#[async_trait::async_trait]
impl RawTool for SandboxDeclaration {
    fn id(&self) -> &str {
        self.0
    }
    fn execution_target(&self) -> ToolExecutionTarget {
        ToolExecutionTarget::Sandbox
    }
    async fn invoke(&self, _: ToolCall) -> Result<ToolOutput, ToolError> {
        panic!("declared Sandbox tool must not execute in Brain")
    }
}

struct PreparingExecutor {
    preparation: Preparation,
    failing_tool: &'static str,
    parallel: bool,
    invocations: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ToolExecutor for PreparingExecutor {
    async fn execution_domain(&self, call: &ToolCall) -> Result<ToolExecutionDomain, ToolError> {
        if call.tool_id == self.failing_tool {
            match self.preparation {
                Preparation::Unavailable => {
                    return Err(ToolError::UnavailableBeforeDispatch(
                        "sandbox never became Ready".into(),
                    ));
                }
                Preparation::Failed => {
                    return Err(ToolError::DefinitiveFailure("invalid preparation".into()));
                }
                Preparation::Indeterminate => {
                    return Err(ToolError::Indeterminate("unknown preparation".into()));
                }
                Preparation::Invalidated => {
                    return Err(ToolError::StateInvalidatedAfterDispatch(
                        "lost projection".into(),
                    ));
                }
                Preparation::Ready => {}
            }
        }
        Ok(ToolExecutionDomain::Worker {})
    }
    fn concurrency(&self, _: &str, _: &serde_json::Value) -> ToolConcurrency {
        if self.parallel {
            ToolConcurrency::Parallel
        } else {
            ToolConcurrency::Serial
        }
    }
    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        self.invocations.lock().unwrap().push(call.tool_id.clone());
        Ok(ToolOutput::ok(
            &call.call_id,
            format!("{} actually executed", call.tool_id),
        ))
    }
}

struct CallsThenReply {
    calls: AtomicUsize,
    expected_error_id: Option<&'static str>,
}

#[async_trait::async_trait]
impl LlmExecutor for CallsThenReply {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(
                ["a", "b"]
                    .into_iter()
                    .map(|id| ToolCall {
                        call_id: id.into(),
                        tool_id: id.into(),
                        arguments: serde_json::json!({}),
                    })
                    .collect(),
            )
        } else {
            let results: Vec<_> = request
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .filter_map(|b| {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } = b
                    {
                        Some((tool_use_id.as_str(), *is_error))
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(
                results,
                vec![
                    ("a", self.expected_error_id == Some("a")),
                    ("b", self.expected_error_id == Some("b"))
                ],
                "both outcomes reach the model once, in model order"
            );
            AssistantOutput::text("preparation outcome explained")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn tool_domain_preparation_preserves_result_and_fatal_boundaries_in_each_dispatch_mode() {
    // Graph: C1 metadata Ready / known pre-entry unavailable / definitive
    // failure / Indeterminate / invalidated-after-effect; C2 serial / parallel;
    // C3 failing metadata is first / second beside a healthy call in the SAME
    // batch. Constraints: metadata
    // failure cannot consume executor admission or claim an execution; fatal
    // evidence cannot become a model-visible ordinary error or allow a retry.
    // Decision table (each row crossed with both C2 and C3 values, 20 cases):
    // R1 Ready => A+B enter once, two successful results, NaturalEnd and ACK.
    // R2 unavailable or definitive failure => failed call never enters;
    // healthy call enters once; both results published; NaturalEnd and ACK.
    // R3 Indeterminate => no failing/later invoke or next inference/publication;
    // logical Indeterminate and monotonic physical ACK veto.
    // R4 invalidated => no failing/later invoke or next inference; StateConflict,
    // not an ordinary error result. Existing post-entry tests own completed
    // effect semantics; this table proves the metadata boundary preserves them.
    // This regression failed R2 with Error::Execution and a Requested batch,
    // leaving the public Session Running forever instead of explaining failure.
    // R5 fatal second + serial => retain earlier healthy call's completed state;
    // fatal second + parallel => no invocation before the whole wave's metadata
    // admission finishes. Earlier metadata success is NOT executor entry.
    for parallel in [false, true] {
        for failing_tool in ["a", "b"] {
            for preparation in [
                Preparation::Ready,
                Preparation::Unavailable,
                Preparation::Failed,
                Preparation::Indeterminate,
                Preparation::Invalidated,
            ] {
                let fatal = matches!(
                    preparation,
                    Preparation::Indeterminate | Preparation::Invalidated
                );
                let expected_error = !matches!(preparation, Preparation::Ready);
                let model = Arc::new(CallsThenReply {
                    calls: AtomicUsize::new(0),
                    expected_error_id: expected_error.then_some(failing_tool),
                });
                let executor = Arc::new(PreparingExecutor {
                    preparation,
                    failing_tool,
                    parallel,
                    invocations: Mutex::new(Vec::new()),
                });
                let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
                let mut request = activation("catalog-a");
                request.snapshot.resolved_spec.tool_descriptors = ["a", "b"]
                    .into_iter()
                    .map(|id| {
                        ToolDescriptor::pinned("test", id, id, serde_json::json!({"type":"object"}))
                    })
                    .collect();
                let context = RuntimeRunContext::new()
                    .with_reader(durable.clone())
                    .with_commit(durable.clone())
                    .with_tool_executor(executor.clone());
                let quiescence = context.physical_quiescence.clone();
                let result = Runtime::new()
                    .with_llm(model.clone())
                    .with_tool(Arc::new(SandboxDeclaration("a")))
                    .with_tool(Arc::new(SandboxDeclaration("b")))
                    .execute(request, context)
                    .await
                    .unwrap();
                let expected = match preparation {
                    Preparation::Indeterminate => EndCause::Indeterminate,
                    Preparation::Invalidated => EndCause::Error(Failure::StateConflict),
                    _ => EndCause::NaturalEnd,
                };
                assert_eq!(
                    result,
                    RunState::Ended(expected),
                    "{preparation:?} parallel={parallel} failing={failing_tool}"
                );
                assert_eq!(
                    model.calls.load(Ordering::SeqCst),
                    if fatal { 1 } else { 2 }
                );
                let mut invocations = executor.invocations.lock().unwrap().clone();
                invocations.sort();
                assert_eq!(
                    invocations,
                    if fatal {
                        if !parallel && failing_tool == "b" {
                            vec!["a".to_string()]
                        } else {
                            vec![]
                        }
                    } else if expected_error {
                        vec![if failing_tool == "a" { "b" } else { "a" }.to_string()]
                    } else {
                        vec!["a".to_string(), "b".to_string()]
                    }
                );
                assert_eq!(
                    quiescence.can_acknowledge_return(),
                    !matches!(preparation, Preparation::Indeterminate)
                );
                let batches: Vec<ToolBatch> = durable
                    .committed_state(&ThreadId("thread-1".into()))
                    .into_iter()
                    .filter_map(|c| {
                        if c.key.0 == ActiveToolBatch::KEY
                            && let awaken_agent_contract::agent::state::Action::Set(value) =
                                c.action
                        {
                            return Some(serde_json::from_value(value).unwrap());
                        }
                        None
                    })
                    .collect();
                let last = batches.last().unwrap();
                assert_eq!(last.phase(), ToolBatchPhase::Finalized);
                if fatal {
                    // Terminal sealing is the existing owner: it closes Requested
                    // calls as unconsumed, without inventing an ordinary ToolResult.
                    assert!(
                        last.calls()
                            .iter()
                            .filter(|c| c.call.tool_id == failing_tool)
                            .all(|c| c.result_messages.is_empty())
                    );
                }
                if expected_error {
                    assert!(
                        batches.iter().all(|b| b
                            .calls()
                            .iter()
                            .filter(|c| c.call.tool_id == failing_tool)
                            .all(|c| c.execution_domain.is_none())),
                        "failed metadata never publishes an executor entry"
                    );
                }
                assert_eq!(
                    durable
                        .committed()
                        .run_facts
                        .iter()
                        .filter(|f| matches!(f.state, RunState::Ended(_)))
                        .count(),
                    1,
                    "one terminal fact, no transient execution retry"
                );
            }
        }
    }
}
