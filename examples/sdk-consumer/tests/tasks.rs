//! Consumer task acceptance: public dependencies only, outside the main workspace.
use awaken_runtime::model::{
    AssistantOutput, ChatRequest, ChatResponse, DeltaSink, LlmExecutor, ToolCall,
};
use awaken_runtime::permission::{GateDecision, GateOutcome, ToolGateHook};
use awaken_runtime::tools::{Tool, ToolError, ToolRegistration};
use awaken_runtime::{
    CancellationToken, EndCause, ExecutableAgentSnapshot, ModelBinding, ResumeResult, RunOutcome,
    RunState, Runtime, RuntimeRunContext,
};
use awaken_sdk_consumer::DemoModel;
use awaken_store_inmem::{MemoryCommitCoordinator, MemoryStreamSink};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn agent() -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot::builder("consumer")
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .build()
}
fn memory() -> Arc<MemoryCommitCoordinator> {
    Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()))
}

#[tokio::test]
async fn single_turn_exposes_identity_state_and_committed_output() {
    let store = memory();
    let runtime = Runtime::new().with_llm(Arc::new(DemoModel));
    let outcome = runtime
        .run_with_outcome(
            &agent(),
            "Hello",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .await
        .unwrap();
    assert_eq!(outcome.end_cause(), Some(&EndCause::NaturalEnd));
    assert!(!outcome.run_id().0.is_empty());
    assert!(
        outcome
            .transcript(store.as_ref())
            .unwrap()
            .iter()
            .any(|message| message.text_content() == "Hello from the SDK")
    );
}

#[tokio::test]
async fn multiple_turns_share_one_store_and_thread() {
    let store = memory();
    let context = RuntimeRunContext::new().with_store(store.clone());
    let runtime = Runtime::new().with_llm(Arc::new(DemoModel));
    let first = runtime
        .session(&agent(), "conversation", context.clone())
        .unwrap()
        .run("first")
        .await
        .unwrap();
    let second = runtime
        .session(&agent(), "conversation", context)
        .unwrap()
        .run("second")
        .await
        .unwrap();
    assert_eq!(first.thread_id(), second.thread_id());
    assert_ne!(first.run_id(), second.run_id());
    let transcript = second.transcript(store.as_ref()).unwrap();
    assert_eq!(transcript.len(), 4);
    assert_eq!(transcript[0].text_content(), "first");
    assert_eq!(transcript[2].text_content(), "second");
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    text: String,
}
struct Echo(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for Echo {
    type Args = EchoArgs;
    type Output = String;
    const ID: &'static str = "consumer.echo";
    const DESCRIPTION: &'static str = "Echo text";
    async fn call(&self, args: EchoArgs) -> Result<String, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(format!("echoed: {}", args.text))
    }
}
struct ToolModel;
#[async_trait::async_trait]
impl LlmExecutor for ToolModel {
    async fn infer(&self, request: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        let output = if request
            .messages
            .iter()
            .any(|message| message.text_content().contains("echoed: hello"))
        {
            AssistantOutput::text("complete")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "echo-1".into(),
                tool_id: Echo::ID.into(),
                arguments: serde_json::json!({"text":"hello"}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}
fn tool_runtime(hits: Arc<AtomicUsize>) -> (Runtime, ExecutableAgentSnapshot) {
    let registration = ToolRegistration::new("consumer", Echo(hits)).unwrap();
    let runtime = registration
        .install(Runtime::new().with_llm(Arc::new(ToolModel)))
        .unwrap();
    let descriptor = registration.descriptor().clone();
    let agent = ExecutableAgentSnapshot::builder("tools")
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .tool(descriptor)
        .max_steps(8)
        .build();
    (runtime, agent)
}

#[tokio::test]
async fn one_typed_tool_definition_supplies_schema_and_execution() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (runtime, agent) = tool_runtime(hits.clone());
    let store = memory();
    let outcome = runtime
        .run_with_outcome(
            &agent,
            "echo",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .await
        .unwrap();
    assert_eq!(outcome.end_cause(), Some(&EndCause::NaturalEnd));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(
        outcome
            .transcript(store.as_ref())
            .unwrap()
            .iter()
            .any(|message| message.text_content().contains("echoed: hello"))
    );
}

struct StreamingModel;
#[async_trait::async_trait]
impl LlmExecutor for StreamingModel {
    async fn infer(&self, _: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        panic!("streaming entry expected")
    }
    async fn infer_streaming(
        &self,
        _: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> awaken_runtime::model::Result<ChatResponse> {
        for chunk in ["Hello ", "世界"] {
            sink.on_text(chunk).await;
        }
        Ok(ChatResponse {
            output: AssistantOutput::text("Hello 世界"),
            usage: None,
            stop_reason: None,
        })
    }
}
#[tokio::test]
async fn streaming_delivers_progress_and_commits_complete_unicode_output() {
    let runtime = Runtime::new().with_llm(Arc::new(StreamingModel));
    let store = memory();
    let sink = Arc::new(MemoryStreamSink::new());
    let outcome = runtime
        .run_with_outcome(
            &agent(),
            "stream",
            RuntimeRunContext::new()
                .with_store(store.clone())
                .with_stream_sink(sink.clone()),
        )
        .await
        .unwrap();
    assert_eq!(outcome.end_cause(), Some(&EndCause::NaturalEnd));
    assert!(!sink.events().is_empty());
    assert!(
        outcome
            .transcript(store.as_ref())
            .unwrap()
            .iter()
            .any(|message| message.text_content() == "Hello 世界")
    );
}

#[tokio::test]
async fn cancelled_input_never_enters_the_model() {
    let runtime = Runtime::new().with_llm(Arc::new(StreamingModel));
    let store = memory();
    let token = CancellationToken::new();
    token.cancel();
    let outcome = runtime
        .run_with_outcome(
            &agent(),
            "cancel",
            RuntimeRunContext::new()
                .with_store(store.clone())
                .with_cancellation(token),
        )
        .await
        .unwrap();
    assert_eq!(outcome.end_cause(), Some(&EndCause::Cancelled));
}

struct ApprovalGate;
#[async_trait::async_trait]
impl ToolGateHook for ApprovalGate {
    async fn gate(&self, _: &ToolCall, _: &awaken_runtime::permission::Store) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::RequireConfirmation {
            correlation_id: "consumer-approval".into(),
        })
    }
}

#[tokio::test]
async fn asynchronous_approval_resumes_the_committed_ticket() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (runtime, agent) = tool_runtime(hits.clone());
    let clock = awaken_time::system_clock();
    let runtime = runtime
        .with_clock(clock.clone())
        .with_gate(Arc::new(ApprovalGate));
    let store = Arc::new(MemoryCommitCoordinator::new(clock));
    let state = runtime
        .session(
            &agent,
            "approval",
            RuntimeRunContext::new().with_store(store),
        )
        .unwrap()
        .run_to_completion_async("echo", |ticket| async move {
            assert_eq!(ticket.correlation_id, "consumer-approval");
            tokio::task::yield_now().await;
            Ok(ResumeResult::allow())
        })
        .await
        .unwrap()
        .state()
        .clone();
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn filesystem_restart_reads_receipt_and_replays_terminal_without_a_model() {
    let directory = tempfile::tempdir().unwrap();
    let clock = awaken_time::system_clock();
    let store = Arc::new(
        awaken_store_fs::FsCommitCoordinator::open(directory.path(), clock.clone())
            .await
            .unwrap(),
    );
    let runtime = Runtime::new().with_llm(Arc::new(DemoModel));
    let outcome = runtime
        .session(
            &agent(),
            "durable",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap()
        .run("first")
        .await
        .unwrap();
    drop(store);
    drop(runtime);
    let reopened = Arc::new(
        awaken_store_fs::FsCommitCoordinator::open(directory.path(), clock)
            .await
            .unwrap(),
    );
    let receipt =
        RunOutcome::read(reopened.as_ref(), outcome.run_id(), outcome.thread_id()).unwrap();
    assert_eq!(receipt, outcome);
    let replay = Runtime::new()
        .session(
            &agent(),
            receipt.thread_id().0.clone(),
            RuntimeRunContext::new().with_store(reopened.clone()),
        )
        .unwrap()
        .resume_with_id_async(receipt.run_id().clone(), "do not append", |_| async {
            panic!("already terminal")
        })
        .await
        .unwrap()
        .state()
        .clone();
    assert_eq!(replay, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(receipt.transcript(reopened.as_ref()).unwrap().len(), 2);
}
#[test]
fn web_policy_configuration_remains_usable_through_sdk_reexports() {
    use awaken_agent_contract::ToolsetSource;
    use awaken_ext_builtin_tools::{
        WebDomainFilter, WebFetchExecutionConfiguration, WebToolConfigurationCompiler,
    };
    use awaken_tool_contract::{ToolConfigurationCompiler, ToolConfigurationInput};
    use awaken_web_policy_contract::{
        EffectiveWebFetchConfiguration, WebAdmissionFailure, WebExecutionConfiguration,
        admit_fetch_url,
    };
    use serde_json::json;

    // A downstream consumer can still construct the old exported types; these
    // are the kernel's actual types, not lookalike adapter DTOs.
    let old_path = WebFetchExecutionConfiguration {
        domains: Some(WebDomainFilter::Allow(vec!["example.com".into()])),
        max_content_tokens: Some(0),
        ..Default::default()
    };
    let kernel_type: awaken_web_policy_contract::WebFetchExecutionConfiguration = old_path;
    let canonical = WebExecutionConfiguration::WebFetch(kernel_type.clone())
        .compile(None)
        .unwrap();
    assert_eq!(
        canonical,
        json!({"type":"web_fetch","domains":{"type":"allow","domains":["example.com"]},"max_content_tokens":0})
    );
    let effective = EffectiveWebFetchConfiguration::compose(
        Some(kernel_type),
        [WebFetchExecutionConfiguration {
            domains: Some(WebDomainFilter::Block(vec!["private.example.com".into()])),
            max_content_tokens: Some(100),
            ..Default::default()
        }],
    )
    .unwrap()
    .unwrap();
    assert_eq!(effective.max_content_tokens, Some(0));
    assert!(
        admit_fetch_url(
            "https://www.example.com/path",
            &effective.domains,
            &[],
            None
        )
        .is_ok()
    );
    assert_eq!(
        admit_fetch_url(
            "https://private.example.com/",
            &effective.domains,
            &[],
            None
        ),
        Err(WebAdmissionFailure::DomainRejected)
    );
    // External implementations still use the existing public provider method
    // and legacy exported filter path after the kernel's ownership move.
    struct ExternalFetch;
    #[async_trait::async_trait]
    impl awaken_ext_builtin_tools::WebFetchProvider for ExternalFetch {
        fn descriptor(&self) -> awaken_ext_builtin_tools::WebFetchProviderDescriptor {
            awaken_ext_builtin_tools::WebFetchProviderDescriptor {
                id: "consumer-fetch".into(),
                label: "Consumer".into(),
                credential: awaken_ext_builtin_tools::WebSearchCredentialRequirement::None,
                options_schema: json!({"type":"object"}),
            }
        }
        async fn fetch(
            &self,
            request: awaken_ext_builtin_tools::WebFetchRequest,
            _: Option<&awaken_agent_contract::credential::Credential>,
            _: &[awaken_ext_builtin_tools::WebDomainFilter],
        ) -> Result<String, awaken_tool_contract::tool::ToolError> {
            Ok(request.url)
        }
    }
    let provider: &dyn awaken_ext_builtin_tools::WebFetchProvider = &ExternalFetch;
    assert_eq!(provider.descriptor().id, "consumer-fetch");
    let compiler = WebToolConfigurationCompiler;
    assert_eq!(
        compiler
            .compile(ToolConfigurationInput {
                source: &ToolsetSource::Agent,
                tool_name: "web_fetch",
                configuration: &canonical,
                client_tools: &[],
            })
            .unwrap(),
        canonical
    );
    let opaque = json!({"type":"future","url_sources":17,"private":{"x":null}});
    assert_eq!(
        compiler
            .compile(ToolConfigurationInput {
                source: &ToolsetSource::Mcp {
                    server_name: "external".into()
                },
                tool_name: "web_fetch",
                configuration: &opaque,
                client_tools: &[],
            })
            .unwrap(),
        opaque
    );
    let malformed = json!({"type":"web_fetch","domains":{"type":"allow","domains":[]}});
    let error = compiler
        .compile(ToolConfigurationInput {
            source: &ToolsetSource::Agent,
            tool_name: "web_fetch",
            configuration: &malformed,
            client_tools: &[],
        })
        .unwrap_err();
    assert_eq!(error.code, "invalid_web_domain_count");
    assert_eq!(error.path, "domains.domains");
}
