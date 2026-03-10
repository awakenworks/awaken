# Build an Agent

Use this when you need a production integration path with tool registry, persistence, and protocol endpoints.

## Prerequisites

- One model provider key is configured (for example `OPENAI_API_KEY` for `gpt-4o-mini`).
- You have at least one tool implementation.
- You know whether the deployment needs persistent storage.

## Steps

1. Define tool set.

```rust,ignore
.with_tools(tool_map([SearchTool, SummarizeTool]))
```

2. Define agent behavior.

```rust,ignore
.with_agent_spec(AgentDefinitionSpec::local_with_id(
    "assistant",
    AgentDefinition::new("gpt-4o-mini")
        .with_system_prompt("You are a helpful assistant.")
        .with_max_rounds(10)
        .with_allowed_tools(vec!["search".to_string(), "summarize".to_string()]),
))
```

3. Wire persistence.

```rust,ignore
.with_agent_state_store(store.clone())
```

4. Execute via `run_stream`.

```rust,ignore
let run = os.run_stream(RunRequest {
    agent_id: "assistant".to_string(),
    thread_id: Some("thread-1".to_string()),
    run_id: None,
    parent_run_id: None,
    parent_thread_id: None,
    resource_id: None,
    origin: RunOrigin::default(),
    state: None,
    messages: vec![Message::user("hello")],
    initial_decisions: vec![],
}).await?;
```

5. Consume stream and inspect terminal state.

```rust,ignore
let mut events = run.events;
while let Some(event) = events.next().await {
    if let AgentEvent::RunFinish { termination, .. } = event {
        println!("termination = {:?}", termination);
    }
}
```

## Verify

- You receive at least one `RunStart` and one `RunFinish` event.
- `RunFinish.termination` matches your expectation (`NaturalEnd`, `Stopped`, `Error`, etc.).
- If persistence is enabled, thread can be reloaded from store after run.

## After The Agent Is Built

Once you have:

```rust,ignore
let os = AgentOsBuilder::new()
    .with_tools(...)
    .with_agent_spec(...)
    .build()?;
```

you normally choose one of these runtime modes:

1. In-process execution: call `os.run_stream(RunRequest { ... }).await?`
2. Long-lived backend service: put `Arc<AgentOs>` into server state and expose HTTP protocol routes
3. Example/starter backend: reuse the same builder pattern in a dedicated binary and let frontend clients connect over AI SDK or AG-UI

The important point is that `AgentDefinition` creation alone does not "start" anything. The run starts only when:

- your code calls `run_stream(...)`, or
- an HTTP route receives a request and delegates to `AgentOs`

## Common Errors

- Model/provider mismatch:
  Use a model id compatible with the provider key you exported.
- Tool unavailable:
  Ensure tool id is registered and included in `allowed_tools` if whitelist is enabled.
- Empty runs with no meaningful output:
  Confirm user message is appended in `RunRequest.messages`.

## Related Example

- `examples/ai-sdk-starter/README.md` is the fastest browser-facing backend integration
- `examples/copilotkit-starter/README.md` shows the same runtime exposed through AG-UI with richer UI state

## Key Files

- `examples/src/starter_backend/mod.rs`
- `crates/tirea-agentos/src/composition/agent_definition.rs`
- `crates/tirea-agentos/src/composition/builder.rs`
- `crates/tirea-agentos-server/src/main.rs`

## Related

- [Expose HTTP SSE](./expose-http-sse.md)
- [Expose NATS](./expose-nats.md)
- [Integrate AI SDK Frontend](./integrate-ai-sdk-frontend.md)
- [Integrate CopilotKit (AG-UI)](./integrate-copilotkit-ag-ui.md)
- [Run Lifecycle and Phases](../explanation/run-lifecycle-and-phases.md)
