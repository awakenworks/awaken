//! Actual Native fresh/client-resume delivery of Web provenance, using the
//! existing SDK snapshot builder, committed ticket and in-memory coordinator.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

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

struct Model(AtomicUsize);
#[async_trait::async_trait]
impl LlmExecutor for Model {
    async fn infer(&self, _: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = match self.0.fetch_add(1, Ordering::SeqCst) {
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
    // client cannot satisfy server-only, with zero fetch provider calls. All
    // rows run the real phase/commit/resume path, not manually supplied State.
    for (client, allowed_source, expected_hits) in [
        (false, "server", 1),
        (true, "client", 1),
        (true, "server", 0),
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
        let mut runtime = Runtime::new()
            .with_clock(clock.clone())
            .with_llm(Arc::new(Model(AtomicUsize::new(0))))
            .with_plugin(Arc::new(fetch));
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
            .plugins(
                sections
                    .iter()
                    .map(|(id, _): &(String, serde_json::Value)| id.clone()),
            )
            .plugin_config(sections)
            .build();
        let store = Arc::new(MemoryCommitCoordinator::new(clock));
        let completed = runtime
            .session(
                &snapshot,
                "sources",
                RuntimeRunContext::new().with_store(store.clone()),
            )
            .unwrap()
            .run_to_completion_async("lookup", |ticket| async move {
                assert!(client, "Native search never awaits an external result");
                Ok(ResumeResult::ToolResult(ToolOutput::ok(
                    ticket.call_id().expect("client call"),
                    URL,
                )))
            })
            .await
            .unwrap();
        assert!(completed.state().is_terminal());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            expected_hits,
            "client={client}, allowed={allowed_source}"
        );
    }
}
