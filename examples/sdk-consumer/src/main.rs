use awaken_runtime::{ExecutableAgentSnapshot, ModelBinding, Runtime, RuntimeRunContext};
use awaken_sdk_consumer::DemoModel;
use awaken_store_inmem::MemoryCommitCoordinator;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::new().with_llm(Arc::new(DemoModel));
    let agent = ExecutableAgentSnapshot::builder("assistant")
        .model(ModelBinding::parse("demo", "stub", "stub")?)
        .build();
    let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let outcome = runtime
        .run_with_outcome(
            &agent,
            "Hello",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .await?;
    println!("Run {:?}: {:?}", outcome.run_id(), outcome.end_cause());
    for message in outcome.transcript(store.as_ref())? {
        println!("{}", message.text_content());
    }
    Ok(())
}
