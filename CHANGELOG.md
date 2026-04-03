# Changelog

All notable changes to this project will be documented in this file.
Format follows [Keep a Changelog](https://keepachangelog.com/). Versions use [Semantic Versioning](https://semver.org/).

## [Unreleased]

Development work lands here. Before releasing, move these items to a versioned section.

## [0.1.0] - 2026-04-03

### Core Runtime

- Phase pipeline with 8 lifecycle hooks: `RunStart`, `StepStart`, `BeforeInference`, `AfterInference`, `BeforeToolExecute`, `AfterToolExecute`, `StepEnd`, `RunEnd`
- Plugin system with typed effect handlers, scheduled actions, and commit hooks for state mutation
- `AgentResolver` trait for dynamic agent resolution with composite and config-backed registry implementations
- `StopPolicy` trait with built-in policies: max rounds, token budget, wall-clock timeout, consecutive errors
- Agent handoff extension for same-thread dynamic agent switching without run termination
- Background task extension for spawning, tracking, and cancelling long-running tasks across runs
- Sub-agent delegation via `AgentTool` with local and remote (A2A) backends
- Context compaction with configurable summarization and artifact truncation to manage long conversations
- LLM retry policy with exponential backoff, per-model circuit breaker, and ordered fallback model list
- `AgentRuntimeBuilder` for ergonomic runtime assembly with plugins, tools, and registry wiring
- `StateStore` with typed slot access, snapshot, and batched mutation commits

### Contract Types

- `LlmExecutor` trait for provider-neutral streaming inference with `genai` as the default backend
- `Tool` and `TypedTool` traits with JSON Schema descriptors and `ToolCallContext`
- `ToolBehaviorBundle` for grouping tools and plugin references by bundle ID
- `Suspension` and `ResumeDecisionAction` for human-in-the-loop tool approval flows
- `MailboxJob`, `MailboxStore`, and `MailboxInterrupt` for persistent run queuing with lease semantics
- `AgentSpec` and `RemoteEndpoint` types for declarative agent configuration in `awaken.toml`
- `AgentCard` with auth descriptor for A2A discovery
- `Phase` enum with `is_run_level` / `is_step_level` classification helpers
- `InferenceRequestTransform` trait for pre-inference prompt mutation
- `ToolInterceptPayload` for plugin-level tool call interception before execution

### Protocols

- AI SDK v6 HTTP+SSE protocol (`/v1/ai-sdk/...`) for Vercel AI SDK frontend integration
- AG-UI protocol (`/v1/ag-ui/...`) with CopilotKit frontend support
- Google A2A protocol (`/v1/a2a/...`) including agent card discovery, task send/status/cancel, and `message:send`
- ACP protocol with both HTTP and stdio transports
- MCP Streamable HTTP transport (`POST /v1/mcp`, `GET /v1/mcp`) and stdio transport for tool exposure
- SSE reconnection replay buffer so clients can catch up on missed events after a disconnect
- Thread and run REST API (`/v1/threads`, `/v1/runs`) with pagination, metadata patch, and cancel endpoints
- HITL decision endpoints: `POST /v1/threads/:id/decision` and `POST /v1/runs/:id/decision`
- Mailbox dispatch endpoint (`/v1/threads/:id/mailbox`) for enqueuing background jobs
- Health check endpoint (`/health`) with per-component store liveness reporting

### Storage

- `InMemoryStore` for threads, messages, and runs — suitable for testing and ephemeral deployments
- `FileStore` with crash-safe atomic writes and `.checkpoint_pending` recovery on restart
- `PostgresStore` for production thread, message, and run persistence
- `InMemoryMailboxStore` for lease-based job queuing in tests and local development
- `ProfileStore` trait for per-agent profile persistence with `ProfileOwner` scoping

### Extensions / Plugins

- `PermissionPlugin`: declarative allow/deny/ask rules with glob and regex matching on tool names and arguments, firewall-style priority (Deny > Allow > Ask)
- `ObservabilityPlugin`: per-inference and per-tool metrics aligned with OpenTelemetry GenAI semantic conventions; pluggable `MetricsSink` with in-memory, batching, persistent, composite, and OTel backends
- `DeferredToolsPlugin`: lazy tool loading with `ToolSearch` tool for runtime capability discovery
- `SkillsPlugin`: filesystem and compile-time embedded skills with markdown frontmatter, `FsSkill` discovery, and optional MCP bridge
- `ReminderPlugin`: declarative post-tool-execution context injection rules with input/output pattern matching
- `A2uiPlugin` (generative UI): streaming sub-agent UI rendering via OpenUI Lang with JSON render and data model updates
- `CompactionPlugin` and `ContextTransformPlugin`: automatic context window management with configurable summarization LLM and artifact compaction thresholds
- `HandoffPlugin`: zero-cost in-loop agent variant switching with per-variant system prompt, model, and tool overlays
- `BackgroundTaskPlugin`: long-lived task lifecycle with status queries and cancellation exposed as agent tools

### Developer Experience

- `awaken` facade crate with `full` feature flag and granular opt-in features: `permission`, `observability`, `mcp`, `skills`, `reminder`, `server`, `generative-ui`
- `awaken::prelude` re-export for ergonomic imports
- Example: generative UI parent/sub-agent delegation pipeline
- Example: AI SDK starter with Next.js frontend wiring
- Example: CopilotKit AG-UI starter
- `awaken-doctest` crate for compile-checked documentation examples
- TOML-based agent configuration (`awaken.toml`) with JSON Schema for editor validation
