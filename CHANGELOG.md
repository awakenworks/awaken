# Changelog

All notable changes to this project will be documented in this file.
Format follows [Keep a Changelog](https://keepachangelog.com/). Versions use [Semantic Versioning](https://semver.org/).

## [Unreleased]

Development work lands here before the next versioned release.

### Changed

- Tool catalog semantics: `AgentSpec.allowed_tools` / `excluded_tools` now use explicit catalog
  values: `["*"]` for allow-all, `[]` for block-none on `excluded_tools`, and
  tool-id patterns matched by `awaken_tool_pattern::tool_id_match`. Existing
  catalog entries containing an unescaped `*` or a literal `\` change from
  exact-string matching to pattern matching; escape literal stars as `\*`
  and literal backslashes as `\\` when a real tool id contains either
  character.

## [0.5.0] - 2026-05-10

The headline work since 0.4.0: every config record now flows through a CAS
envelope with field-level patches and read-time merge; built-in agents,
models, providers, MCP servers, and tools become customisable without
forking; an admin audit log captures who changed what and supports one-click
restore; provider authentication is centralised in a `CredentialBroker` with
unified retry and Vertex SA-JSON support; per-agent runtime stats and a
fixture-driven eval framework land alongside an Admin Console rebuild
covering tabs, ⌘K, dark mode, mobile, Chinese locale, and history.

### Added — Configuration data plane (`awaken-contract`)

- `ConfigRecord<T>` / `RecordMeta` / `RecordSource` envelope plus
  `decode_config_record`, `effective_config_record`,
  `effective_visible_config_records`, and `ConfigRecordError` for layering
  user overrides on top of built-in seeds.
- `AgentSpecPatch` + `merge_agent_spec` for field-level overrides on
  built-in agents. Tri-state semantics: missing inherits, JSON `null`
  clears, JSON value overrides; `#[serde(deny_unknown_fields)]` rejects
  drift; per-key shallow merge for `sections`.
- `ToolSpecPatch` + `merge_tool_spec` and a new `ToolSpec` type for the
  CAS-backed tools config surface.
- `BuiltinSeedSet` + `BuiltinSpec` for declarative seed manifests that the
  server ingests on boot.
- `validate_agent_spec`, `validate_agent_spec_patch`, `validate_provider_spec`,
  `validate_model_binding_spec`, `validate_config_record`, plus
  `UnknownFieldPolicy` and `ConfigValidationError`. Per-type policy
  constants (`AGENT_SPEC_UNKNOWN_FIELD_POLICY`, etc.) let callers pin
  forward-compatibility behaviour.
- `AuditEvent` / `AuditAction` types per ADR-0026, including the `Restore`
  action and `restored_from` reference.

### Added — Server (`awaken-server`)

- `AuditLogger` service with `emit` / `query` / `prune_before`; `ConfigService`
  and `AppState` thread audit through every mutation (including MCP restart).
  `AdminApiConfig` exposes `audit_*` knobs for retention sweeper and TTL.
- `GET /v1/audit-log` with pagination, filters, and stable cursor;
  `POST /v1/config/:namespace/:id/restore` rolls a record back to a prior
  version and emits a `Restore` event with `restored_from`.
- Patch-overrides write path with read-time merge; built-in record overrides
  are stored as `AgentSpecPatch` / `ToolSpecPatch` JSON inside `RecordMeta`.
- Seed-driven boot: spec registry hydrates from `BuiltinSeedSet` and merges
  with stored overrides on every read.
- `GET /v1/system/info` and `POST /v1/config/:ns/validate` for admin-side
  preflight.
- `POST /v1/providers/:id/test` to validate provider credentials end-to-end.
- MCP server status (`GET /v1/mcp-servers/:id/status`) and restart
  (`POST /v1/mcp-servers/:id/restart`) endpoints, with health budget
  enforcement.
- `DELETE /v1/:resource/:id` returns `409 used_by: [...]` listing dependent
  records; `?force=true` bypasses for explicit cascade.
- Probe-based adapter discovery via `AdapterKind::from_lower_str` covers all
  `genai` adapter variants without per-variant wiring.
- `created_at` / `updated_at` are tracked on every config record.

### Added — Runtime credentials (`awaken-runtime`)

- `CredentialBroker` unifies provider authentication via `ProviderSpec`. The
  broker absorbs transient credential errors with bounded retry and emits
  observability spans for mint / refresh / failure.
- `Minter` trait abstracts credential acquisition (API key, Vertex
  service-account JSON, etc.). New `vertex` minter accepts SA-JSON inline
  via `credentials_kind`.
- Shared retry primitive used by both inference and credential paths.

### Added — Runtime registry & lifecycle

- Registry lifecycle APIs (`reload`, `preview`, `apply`) with provider
  preflight and single-scan provider preview.
- Non-blocking provider update path: long-running `Provider::build` no longer
  stalls catalog reads.

### Added — Observability (`awaken-ext-observability`)

- `RuntimeStatsRegistry` per-agent rolling window (22 tests); fed by both the
  inference and tool pipelines.
- `AgentToolStats` and `stats_by_agent_and_tool` aggregations.
- Latency histogram with p99 / min / max plus per-tool percentiles.
- Trace attributes aligned with the OpenTelemetry GenAI semantic conventions
  and Arize Phoenix span model (#181).

### Added — Evaluation (`awaken-eval`, new crate, `publish = false`)

- Fixture-driven replay framework with deterministic `MockReplayer` and a
  runtime-driven replayer that exercises the real agent loop.
- `awaken-eval` CLI with `replay` (NDJSON report) and `check` (baseline diff,
  exit 1 on regression) subcommands.
- Optional `llm-judge` feature with `TensorZeroJudge` for graded scoring.
- Five seed fixtures + integration tests covering the runtime pipeline.
- `ReplayReport.tool_calls_by_agent` field surfaces per-agent tool usage in
  reports consumed by the Admin Console Eval Reports page.

### Added — Admin Console (`apps/admin-console`)

- Foundation: Style Dictionary token pipeline (light + dark), grouped
  sidebar, topbar identity / status, ⌘K command palette, dark mode toggle,
  responsive mobile drawer, full Chinese (`zh-CN`) locale, IA v2.4.
- Tabbed agent editor (Basics / Tools / Plugins / Delegates / Advanced /
  History) with sticky save bar, "Unsaved changes" / "Up to date" badge,
  and `?tab=` mirroring.
- Audit Log page at `/audit-log` with filters, table, event detail panel,
  and URL-encoded filter state.
- History tab in agent editor lists prior versions and offers a confirm-flow
  one-click restore via `restoreConfig`.
- Tools management UI backed by the CAS tools config surface.
- Per-agent runtime dashboard (24 vitest cases) with tool-call panel,
  latency histogram, sparklines, and selectable time range.
- Eval Reports page parses NDJSON (offline file upload), with status filter
  (`passed` / `failed` / `regressions` / `fixed`), per-fixture search, and
  multi-baseline diff.
- Toast queue (`ToastProvider`/`useToast`) with eviction count and Escape
  dismiss, confirm dialog (`ConfirmDialogProvider`/`useConfirmDialog`),
  router-level unsaved-changes guard (`useUnsavedChangesGuard`), and admin
  bearer-token modal (`AdminTokenModal`).
- 401 responses prompt for a fresh admin token and replay the original
  request once the user submits.
- Client-side search, header-click sort, and 10/20/50/100 pagination on
  every catalog page; URL-persistent list state via `lib/list-view.ts` and
  `components/list-controls.tsx`.
- Grouped `ToolSelector` with `All tools` / `Custom` modes, in-group search,
  select-all / clear-group, and `built-in` / `plugin:*` / `mcp:*` grouping;
  exposes the previously hidden `excluded_tools` and `reasoning_effort`
  fields on `AgentSpec`.
- AI Assistant chat renders streamed `tool-*` and `dynamic-tool` parts as
  collapsible cards with status, input, output, and error payloads.
- MCP / Skill detail pages, provider test button, MCP restart button,
  Skills caller / context filters, and `created_at` / `updated_at`
  "Last modified" column.

### Changed

- Admin Console bootstrap migrated from `<BrowserRouter>` to
  `createBrowserRouter` + `<RouterProvider>`. `useBlocker` (used by the
  unsaved-changes guard) requires the data router; the legacy router crashed
  the agent editor with an invariant. `appRoutes()` is exported so tests can
  mount the same tree under `createMemoryRouter`.
- Agent editor tab list now uses an ARIA `tablist` with arrow-key
  navigation; modal dialogs trap focus and require backdrop-click symmetry.
- Provider executors are reused across applies whose `ProviderSpec` is
  unchanged (carried over from 0.4.0 and now combined with the
  CredentialBroker mint cache).
- `subtle 2.6` added to workspace dependencies for constant-time admin token
  comparison.

### Fixed

- WebStream HTTP errors are classified via downcast: 4xx no longer triggers
  inference retry, only true transport / 5xx do.
- Audit log cursor returns `400` (not `500`) on malformed input; sweeper
  emits a single `warn!` per misconfigured interval; emit path deduplicated.
- `AuthProvider` no longer double-probes under React StrictMode.
- Admin Console catalog editors gain client-side validation across all
  create flows with a11y polish.
- Audit Clear refetches the visible page; cmdk entries pick up the active
  locale; form titles, search placeholders, and detail-route breadcrumbs are
  fully internationalised.

### Documentation

- ADR-0024 admin console data router, ADR-0025 config draft state, ADR-0026
  admin audit log, ADR-0027 server-side eval history, ADR-0028 config
  version switching, ADR-0029 tool description override.
- mdBook gains `how-to/use-admin-console.md`, `reference/admin-console.md`,
  `reference/provider-model-config.md`, and a `zh-CN` mirror; `http-api.md`
  documents the new endpoints; `config.md` covers lifecycle and override
  references.
- README and README.zh-CN refreshed with admin-console screenshots.

### Compatibility

- Public API additions to `awaken-contract` are purely additive — no
  re-exports were removed or renamed; the 0.4.0 import surface continues to
  compile.
- `awaken-eval` ships as a workspace crate but is not published to
  crates.io (`publish = false`). Downstream consumers vendor or path-depend
  for now.
- New endpoints (`/v1/audit-log`, `/v1/config/:ns/:id/restore`,
  `/v1/system/info`, `/v1/config/:ns/validate`, MCP status / restart,
  provider test) are gated by `AdminApiConfig.expose_config_routes`.
- `DELETE /v1/:resource/:id` is a behaviour change: existing callers that
  previously assumed unconditional success must now handle `409 used_by`
  or pass `?force=true`.

## [0.4.0] - 2026-04-27

This release adopts the `awaken` crate name on crates.io. Versions 0.1–0.3 of
`awaken` belonged to a separate, now-archived crate maintained by
[@brayniac](https://github.com/brayniac), who generously transferred the name —
thank you. The codebase continues the line that previously shipped as
`awaken-agent`; the 0.4 jump exists only to skip past the prior versions
already published under the `awaken` name. There is no 0.3 release of this
codebase. Rust imports remain `awaken` either way.

The headline work since `awaken-agent 0.2.1`: streaming LLM calls now survive
mid-stream failures and can resume across processes; threads carry a durable
parent-child lineage with explicit child-delete strategies; provider keys and
admin tokens redact themselves through `RedactedString`.

### Added

- New `AgentEvent::ToolCallCancel` and `AgentEvent::StreamReset` variants so
  consumers can drop partial deltas during stream recovery without ambiguity.
- `StreamCheckpointStore` contract for cross-process stream resume (the
  in-process retry loop already covered same-process recovery).
- `ProviderSpec.adapter_options`: a non-secret `BTreeMap<String, Value>` that
  adapters can read for things like custom headers on OpenAI-compatible
  proxies; unknown keys are accepted by the schema but ignored at build time.
- Filtered thread queries (`ThreadQuery`/`ThreadPage`) and message paging
  (`MessageQuery`/`MessagePage`) on `ThreadStore`, consumed by the AG-UI and
  AI SDK v6 protocol routes.
- Thread parent-child lineage: `Thread.parent_thread_id`,
  `RunRequestSnapshot.parent_thread_id`, and
  `ThreadStore::delete_thread_with_strategy` (`reject`/`detach`/`cascade`),
  with backend-native overrides on the file, PostgreSQL, and NATS-buffered
  stores.
- `AdminApiConfig.expose_config_routes` toggle and
  `ConfigRuntimeManager::with_min_apply_interval` for hardening and
  debouncing the admin/config plane. Provider executors are reused across
  applies whose `ProviderSpec` is unchanged.
- `awaken::RedactedString` re-export from the facade so secret-handling code
  no longer needs a direct `awaken-contract` dependency.

### Changed

- `ProviderSpec.api_key`, `AdminApiConfig.bearer_token`, and
  `ServerConfig.a2a_extended_card_bearer_token` are now
  `Option<RedactedString>`. JSON wire format is unchanged. Code that reads
  these fields must call `.expose_secret()`; logging that relied on
  `Debug`/`Display` now prints `***`.
- `InferenceExecutionError` is `#[non_exhaustive]` and splits into retryable
  (`Provider`, `RateLimited`, `Overloaded`, `Timeout`, `StreamInterrupted`),
  permanent (`ContextOverflow`, `InvalidRequest`, `Unauthorized`,
  `ModelNotFound`, `ContentFiltered`), and fail-fast (`AllModelsUnavailable`,
  `Cancelled`) classes. `RateLimited` and `Overloaded` carry an optional
  `retry_after` parsed from `Retry-After`. Prefer `is_retryable()`,
  `counts_toward_circuit_breaker()`, and `retry_after()` over matching
  specific variants.
- Context compaction runs on a background task with single-flight semantics
  and swaps the summary back through the owner inbox; the synchronous
  `compact_with_llm` path is removed.

### Fixed

- NATS buffered thread store now quarantines poison WAL messages with
  bounded NAK and a stable hash, so an unrecoverable entry no longer stalls
  the WAL.
- Background-task spawn commit failures surface through `SpawnError` and a
  metric, rather than silently dropping the spawn.
- Self-cancel commands now cascade across runs in the same lineage.
- Thread-query history ordering hardened across server CI surfaces.

### Compatibility

- Existing serialized config keeps loading; `RedactedString` serializes and
  deserializes as a plain JSON string.
- The 0.4 jump is solely a name-collision skip; the Rust API additions and
  changes are limited to the items above.
- 0.2.0 mailbox and server-facing types remain compatible. The NATS backend
  is additive behind the `awaken-stores/nats` feature.

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
