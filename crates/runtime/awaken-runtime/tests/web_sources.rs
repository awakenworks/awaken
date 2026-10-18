//! Actual Native fresh/client-resume delivery of Web provenance, using the
//! existing SDK snapshot builder and committed tickets in memory and filesystem
//! coordinators, including a fresh Runtime reopened on the same durable store.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_ext_builtin_tools::{
    WebDomainFilter, WebFetchExecutionConfiguration, WebFetchPlugin, WebFetchProvider,
    WebFetchProviderDescriptor, WebFetchRequest, WebSearchCredentialRequirement, WebSearchPlugin,
    WebSearchProvider, WebSearchProviderDescriptor, WebSearchProviderRegistry, WebSearchRequest,
    WebSearchResult, web_fetch_descriptor, web_search_descriptor,
};
use awaken_runtime::{
    ExecutableAgentSnapshot, ModelBinding, ResumeResult, Runtime, RuntimeRunContext,
};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_store_inmem::MemoryCommitCoordinator;
use awaken_tool_contract::tool::{ToolError, ToolOutput};
use serde_json::json;

const URL: &str = "https://example.com/source";

struct Model(AtomicUsize, Arc<AtomicBool>);
#[async_trait::async_trait]
impl LlmExecutor for Model {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let turn = self.0.fetch_add(1, Ordering::SeqCst);
        if turn == 1 {
            self.1.store(
                request.messages.iter().any(|message| {
                    message.content.iter().any(|block| match block {
                        awaken_runtime_contract::ContentBlock::ToolResult { content, .. } => {
                            awaken_runtime_contract::extract_text(content).contains(URL)
                        }
                        _ => false,
                    })
                }),
                Ordering::SeqCst,
            );
        }
        let output = match turn {
            0 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "source".into(),
                tool_id: "web_search".into(),
                arguments: json!({"query":"lookup"}),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "fetch".into(),
                tool_id: "web_fetch".into(),
                arguments: json!({"url":URL}),
            }]),
            _ => AssistantOutput::text("done"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

struct Fetch(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl WebFetchProvider for Fetch {
    fn descriptor(&self) -> WebFetchProviderDescriptor {
        WebFetchProviderDescriptor {
            id: "proof-fetch".into(),
            label: "Proof fetch".into(),
            credential: WebSearchCredentialRequirement::None,
            options_schema: json!({"type":"object"}),
        }
    }
    fn enforces_domain_filter(&self) -> bool {
        true
    }
    async fn fetch(
        &self,
        request: WebFetchRequest,
        _: Option<&awaken_agent_contract::credential::Credential>,
        _: &[WebDomainFilter],
    ) -> Result<String, ToolError> {
        assert_eq!(request.url, URL);
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("fetched".into())
    }
}

struct Search;
#[async_trait::async_trait]
impl WebSearchProvider for Search {
    fn descriptor(&self) -> WebSearchProviderDescriptor {
        WebSearchProviderDescriptor {
            id: "proof-search".into(),
            label: "Proof search".into(),
            credential: WebSearchCredentialRequirement::None,
            options_schema: json!({"type":"object"}),
        }
    }
    async fn search(
        &self,
        _: WebSearchRequest,
        _: Option<&awaken_agent_contract::credential::Credential>,
    ) -> Result<Vec<WebSearchResult>, ToolError> {
        Ok(vec![WebSearchResult {
            title: "document".into(),
            url: URL.into(),
            snippet: "result".into(),
        }])
    }
}

#[tokio::test]
async fn native_and_client_resume_preserve_actual_source_ownership() {
    // R1 real Native search outcome -> server-only fetch admitted; R2 exact
    // committed client ticket resume -> client-only admitted; R3 a same-named
    // client cannot satisfy server-only; R4 KeepLast hides the source result;
    // R5 forged client state is refused and the original ticket resumes after a
    // filesystem restart with its client ownership. Every row runs the actual
    // phase/commit/resume path and asserts zero fetch effects on refusal.
    for (client, allowed_source, expected_hits, hide_result, forge_state) in [
        (false, "server", 1, false, false),
        (true, "client", 1, false, false),
        (true, "server", 0, false, false),
        (false, "server", 0, true, false),
        (true, "server", 0, false, true),
    ] {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut registry = WebSearchProviderRegistry::default();
        registry
            .register_fetch(Arc::new(Fetch(hits.clone())))
            .unwrap();
        registry.register(Arc::new(Search)).unwrap();
        let source = if client {
            ToolDescriptor::client_executed(
                "web_search",
                "Client lookup",
                json!({"type":"object", "properties":{"query":{"type":"string"}}}),
            )
        } else {
            web_search_descriptor()
        };
        let descriptors = vec![source.clone(), web_fetch_descriptor()];
        let settings = serde_json::from_value::<WebFetchExecutionConfiguration>(json!({
            "url_sources": {"user_input":"none", "client_tool_results":if allowed_source=="client" {"all"} else {"none"}, "server_tool_results":if allowed_source=="server" {"all"} else {"none"}}
        })).unwrap();
        let fetch = WebFetchPlugin::new(registry.clone(), None)
            .with_tool_source_catalogue(&descriptors)
            .with_execution_configuration(Some(settings));
        let clock = awaken_time::system_clock();
        let seen_result = Arc::new(AtomicBool::new(false));
        let mut runtime = Runtime::new()
            .with_clock(clock.clone())
            .with_llm(Arc::new(Model(AtomicUsize::new(0), seen_result.clone())))
            .with_plugin(Arc::new(fetch.clone()));
        let mut sections = vec![(
            "web_fetch".into(),
            json!({"provider_id":"proof-fetch", "options":{}}),
        )];
        if !client {
            runtime = runtime.with_plugin(Arc::new(WebSearchPlugin::new(registry, None)));
            sections.push((
                "web_search".into(),
                json!({"provider_id":"proof-search", "options":{}}),
            ));
        }
        let snapshot = ExecutableAgentSnapshot::builder("sources")
            .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
            .tool(source)
            .tool(web_fetch_descriptor())
            .max_steps(4)
            .context_policy(if hide_result {
                awaken_runtime_contract::resolved::ContextPolicy::KeepLast { keep_last: 1 }
            } else {
                awaken_runtime_contract::resolved::ContextPolicy::KeepAll
            })
            .plugins(
                sections
                    .iter()
                    .map(|(id, _): &(String, serde_json::Value)| id.clone()),
            )
            .plugin_config(sections)
            .build();
        let store = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
        let directory = forge_state.then(|| tempfile::tempdir().unwrap());
        let filesystem = match &directory {
            Some(directory) => Some(Arc::new(
                awaken_store_fs::FsCommitCoordinator::open(directory.path(), clock.clone())
                    .await
                    .unwrap(),
            )),
            None => None,
        };
        let context = match &filesystem {
            Some(store) => RuntimeRunContext::new().with_store(store.clone()),
            None => RuntimeRunContext::new().with_store(store.clone()),
        };
        let saved_ticket = Arc::new(Mutex::new(None));
        let callback_ticket = saved_ticket.clone();
        let completed = runtime
            .conversation(
                &snapshot,
                "sources",
                context,
            )
            .unwrap()
            .run_to_completion_async("lookup", move |ticket| {
                let callback_ticket = callback_ticket.clone();
                async move {
                assert!(client, "Native search never awaits an external result");
                *callback_ticket.lock().unwrap() = Some(ticket.clone());
                let mut output = ToolOutput::ok(
                    ticket.call_id().expect("client call"),
                    URL,
                );
                if forge_state {
                    output.state.push(awaken_runtime_contract::StateCommand::set(
                        awaken_runtime_contract::Scope::Thread,
                        awaken_runtime_contract::MergePolicy::Disjoint,
                        "web_fetch.visible_url_sources",
                        json!({"format":3, "outcome_witnesses":{}, "urls":{"ServerTool":{"web_search":[URL]}}, "observed_messages":[]}),
                    ));
                }
                Ok(ResumeResult::ToolResult(output))
                }
            })
            .await;
        if forge_state {
            assert!(
                completed
                    .unwrap_err()
                    .to_string()
                    .contains("client-executed tool results cannot mutate Runtime state")
            );
            assert_eq!(
                hits.load(Ordering::SeqCst),
                0,
                "forged client state cannot reach provider"
            );
            let ticket = saved_ticket.lock().unwrap().take().unwrap();
            drop(runtime);
            drop(filesystem);
            let reopened = Arc::new(
                awaken_store_fs::FsCommitCoordinator::open(
                    directory.as_ref().unwrap().path(),
                    clock.clone(),
                )
                .await
                .unwrap(),
            );
            assert_eq!(
                reopened.resume_ticket(&ticket.run_id),
                Some(ticket.clone()),
                "rejection leaves exact durable ticket"
            );
            let frozen = serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
            let cold = Runtime::new()
                .with_clock(clock.clone())
                .with_llm(Arc::new(Model(AtomicUsize::new(1), seen_result.clone())))
                .with_plugin(Arc::new(fetch));
            cold.register_snapshot(frozen);
            use awaken_time::WallClockExt as _;
            let state = cold
                .resume(
                    awaken_runtime_contract::resume::ResumeCommand::from_ticket(
                        &ticket,
                        ResumeResult::ToolResult(ToolOutput::ok(ticket.call_id().unwrap(), URL)),
                        clock.now_ms(),
                    ),
                    reopened.as_ref(),
                    RuntimeRunContext::new().with_store(reopened.clone()),
                )
                .await
                .unwrap();
            assert!(
                state.is_terminal(),
                "cold valid reply completes original activation"
            );
            assert!(reopened.resume_ticket(&ticket.run_id).is_none());
            assert_eq!(
                hits.load(Ordering::SeqCst),
                0,
                "original client remains unable to satisfy server-only after filesystem restart"
            );
            continue;
        }
        let completed = completed.unwrap();
        assert!(completed.state().is_terminal());
        assert_eq!(
            seen_result.load(Ordering::SeqCst),
            !hide_result,
            "actual model request view"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            expected_hits,
            "client={client}, allowed={allowed_source}"
        );
    }
}
