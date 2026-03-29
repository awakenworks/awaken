//! Multi-turn conversation using AgentRuntime with persistent thread storage.
//!
//! Run with BigModel GLM:
//!   LLM_BASE_URL=https://open.bigmodel.cn/api/paas/v4/ LLM_API_KEY=<key> LLM_MODEL=GLM-4.7-Flash cargo run --example multi_turn
//!
//! Also supports: OPENAI_API_KEY + OPENAI_MODEL, or ANTHROPIC_API_KEY, etc.

use async_trait::async_trait;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::EventSink;
use awaken::contract::message::Message;
use awaken::engine::GenaiExecutor;
use awaken::loop_runner::build_agent_env;
use awaken::registry::ResolvedAgent;
use awaken::stores::InMemoryStore;
use awaken::*;
use std::sync::Arc;

struct ConsoleSink;

#[async_trait]
impl EventSink for ConsoleSink {
    async fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::TextDelta { delta } => print!("{delta}"),
            _ => {}
        }
    }
}

struct SimpleResolver {
    agent: ResolvedAgent,
}

impl AgentResolver for SimpleResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, awaken::RuntimeError> {
        let mut agent = self.agent.clone();
        agent.env = build_agent_env(&[], &agent)?;
        Ok(agent)
    }
}

fn build_llm_executor() -> GenaiExecutor {
    if let (Ok(mut base_url), Ok(api_key)) =
        (std::env::var("LLM_BASE_URL"), std::env::var("LLM_API_KEY"))
    {
        use genai::adapter::AdapterKind;
        use genai::resolver::{AuthData, Endpoint};
        use genai::{ModelIden, ServiceTarget};

        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        let adapter = match std::env::var("LLM_ADAPTER").as_deref() {
            Ok("anthropic") => AdapterKind::Anthropic,
            _ => AdapterKind::OpenAI,
        };
        let client = genai::Client::builder()
            .with_service_target_resolver_fn(move |st: ServiceTarget| {
                Ok(ServiceTarget {
                    endpoint: Endpoint::from_owned(base_url.clone()),
                    auth: AuthData::from_single(api_key.clone()),
                    model: ModelIden::new(adapter, st.model.model_name),
                })
            })
            .build();
        GenaiExecutor::with_client(client)
    } else {
        GenaiExecutor::new()
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let model = std::env::var("LLM_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "gpt-4o-mini".into());
    let llm = Arc::new(build_llm_executor());

    let agent = ResolvedAgent::new(
        "chat",
        &model,
        "You are a helpful assistant. Be concise. Remember what the user tells you.",
        llm,
    );
    let resolver = Arc::new(SimpleResolver {
        agent: agent.clone(),
    });

    let store = Arc::new(InMemoryStore::new());
    let runtime = AgentRuntime::new(resolver).with_thread_run_store(store);

    let thread_id = "conversation-1";

    let turns = vec![
        "My name is Alice. Remember it.",
        "What is the capital of France?",
        "What is my name?",
    ];

    for (i, user_msg) in turns.iter().enumerate() {
        print!(
            "\n[Turn {}] User: {}\n[Turn {}] Assistant: ",
            i + 1,
            user_msg,
            i + 1
        );

        let request =
            RunRequest::new(thread_id, vec![Message::user(*user_msg)]).with_agent_id("chat");

        match runtime.run(request, Arc::new(ConsoleSink)).await {
            Ok(result) => {
                print!(" [{:?}]", result.termination);
            }
            Err(e) => {
                print!(" [ERROR: {e}]");
            }
        }
    }

    print!("\n\n=== Multi-turn test complete ===\n");
}
