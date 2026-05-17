use awaken::contract::message::Message;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, RunRequest};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let spec = AgentSpec::new("assistant")
        .with_model_id("gpt-4o-mini")
        .with_system_prompt("You are helpful.");

    let rt = AgentRuntimeBuilder::new()
        .with_agent_spec(spec)
        .with_provider("openai", Arc::new(GenaiExecutor::new()))
        .with_model_binding(
            "gpt-4o-mini",
            ModelBinding {
                provider_id: "openai".into(),
                upstream_model: "gpt-4o-mini".into(),
            },
        )
        .build()?;

    let req = RunRequest::new("thread-1", vec![Message::user("Hello!")]).with_agent_id("assistant");

    let out = rt.run_to_completion(req).await?;
    println!("{}", out.response);
    Ok(())
}
