# Changelog

All notable changes to this project will be documented in this file.
Format follows [Keep a Changelog](https://keepachangelog.com/). Versions use [Semantic Versioning](https://semver.org/).

## [Unreleased]

Development work lands here before the next versioned release.

## [0.2.1] - 2026-04-21

### Added

- NATS-backed `MailboxStore` and `ThreadRunStore` implementations through the
  `awaken-stores/nats` feature, including JetStream dispatch wakeups, KV-backed
  dispatch state, buffered checkpoint WAL, and read-your-writes overlays.
- Live mailbox steering for active runs, with durable dispatch fallback when no
  live subscriber acknowledges the command.
- NATS mailbox operational metrics, delayed dispatch-signal NAK backoff,
  sweeper republish TTL, configurable signal-loop tuning, and stress coverage
  for multi-node contention and large KV buckets.

### Fixed

- Preserved 0.2.0 public API compatibility for mailbox and server-facing types
  while keeping new dispatch signal behavior additive.
- Hardened distributed mailbox scheduling around thread claim guards,
  `available_at` retry windows, dispatch epoch checks, stale claim rejection,
  dedupe lock reconciliation, and terminal claim-field cleanup.
- Fixed NATS buffered thread flushing so WAL messages are ACKed only after both
  the inner checkpoint and `flushed_seq` watermark write succeed.
- Wired NATS credentials config into both mailbox and buffered thread store
  connections.
- Normalized NATS KV Delete/Purge tombstones across claim guards, dedupe locks,
  thread indexes, epochs, and dispatch control paths.
- Made terminal dispatch GC authoritative and changed expired-lease reclaim to
  use thread-claim guard records instead of scanning all dispatch records.

### Changed

- Updated `genai` to `0.6.0-beta.17`.
- Aligned Rust toolchain metadata with the current workspace MSRV.

### Compatibility

- `cargo semver-checks` passes against `v0.2.0`; this release is intended as a
  non-breaking 0.2 patch release.
- The NATS backend is additive and behind the `awaken-stores/nats` feature.

## [0.2.0] - 2026-04-11

### Breaking Changes

- Agent and model configuration now uses the canonical `model_id` -> `ModelBinding { provider_id, upstream_model }` chain. Legacy `model`, `provider`, `model_name`, and `fallback_models` fields are rejected in managed Awaken config.
- A2A HTTP routes and payloads use the v1 `message:send`, `message:stream`, task wrapper, `supportedInterfaces`, and enum naming shapes. Older `tasks/send` and top-level AgentCard `id`/`url` shapes are not emitted.
- Tool interception should move to `ToolGateHook` via `PluginRegistrar::register_tool_gate_hook()`. `BeforeToolExecute` now represents execution-time hooks only.
- Remote agent endpoints use the canonical `RemoteEndpoint` shape: `backend`, `base_url`, `auth`, `target`, `timeout_ms`, and `options`. Legacy A2A endpoint fields are accepted only as an isolated migration input and cannot be mixed with canonical fields.

### Added

- Canonical `ExecutionBackend` contract for local and remote agent execution, including root execution, delegate execution, abort, remote state continuation, input/auth waits, and output capability reporting.
- Managed configuration API for `agents`, `models`, `providers`, and `mcp-servers`, backed by `ConfigStore` and runtime snapshot validation.
- Admin Console app for editing runtime configuration through the Config API.
- SQLite mailbox store via `SqliteMailboxStore` and the `awaken-stores/sqlite` feature.
- A2A streaming, task subscription, push notification config routes, extended agent cards, and official protocol interop coverage.
- MCP Streamable HTTP session lifecycle, including `MCP-Session-Id`, strict initialize/session validation, streaming `tools/call`, and `DELETE /v1/mcp` session termination.
- OpenUI chat example and expanded generative UI support for A2UI, JSON Render, and OpenUI Lang integrations.

### Changed

- The runtime lifecycle now includes a pure `ToolGate` decision point before tool execution, making permission and interception behavior independent from execution-time side effects.
- Runtime-managed config changes compile and validate a candidate registry snapshot before it replaces the active runtime snapshot.
- A2A remote execution preserves backend task lifecycle state across polling, streaming, interruption, cancellation, and continuation.
- Provider/model retry and inference overrides now operate on upstream model names for the already resolved provider.
- Documentation and examples now use `awaken-agent = 0.2` dependency snippets and canonical model/provider APIs.

### Compatibility

- Existing Rust imports continue to use `awaken` even though the crate is published as `awaken-agent`.
- `awaken_runtime::extensions::a2a` still re-exports compatibility aliases such as `AgentBackend`, `AgentBackendFactory`, and `DelegateRunResult`, but new code should use `ExecutionBackend`, `ExecutionBackendFactory`, and `BackendRunResult`.
- `RemoteEndpoint` deserializes legacy `bearer_token`, `agent_id`, and `poll_interval_ms` only when no canonical fields are present. New serialized config should use `auth`, `target`, and `options`.

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
