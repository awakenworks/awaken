---
title: "Migrate From 0.5 to 0.6"
description: "0.6.0 splits the contract surface and narrows the runtime commit boundary. This guide maps the public API, wire, and storage changes from 0.5.0."
---

0.6.0 is a breaking release for users who implement storage, commit
coordinators, or import low-level contract types directly. The high-level
runtime builder and common tool APIs are still available through `awaken` and
`awaken::prelude::*`, but several 0.5 contract paths and public fields changed.

## Contract Crates

0.5 exposed one contract crate:

```text
awaken-contract
```

0.6 splits that surface:

```text
awaken-runtime-contract  # runtime-facing types and traits
awaken-server-contract   # server/store-facing types and traits
```

Use `awaken-runtime-contract` for tools, inference, events, state, registry
specs, `ThreadCommit`, `ThreadCommitOutcome`, `CommitCoordinator`, and the
runtime checkpoint read port.

Use `awaken-server-contract` for `ThreadQuery`, `MessageQuery`, store traits,
scoped store wrappers, audit/config stores, outbox, protocol replay, versioned
registry, and staged commit outcomes.

The historical `awaken-contract` crate remains as a transition facade, but it
does not preserve every 0.5 module path. Treat the split as breaking and move
imports to the narrower crate you implement against. The `awaken` facade keeps
`awaken::contract::*` as the runtime-facing contract module and exposes
server/store contracts through `awaken::server_contract::*`.

| 0.5 import | 0.6 import |
|---|---|
| `awaken_contract::contract::commit_coordinator::Checkpoint` | `awaken_runtime_contract::contract::commit_coordinator::ThreadCommit` |
| `awaken_contract::contract::commit_coordinator::CheckpointCommitOutcome` | `awaken_runtime_contract::contract::commit_coordinator::ThreadCommitOutcome` |
| `awaken_contract::contract::storage::ThreadQuery` | `awaken_server_contract::contract::storage::ThreadQuery` |
| `awaken::contract::storage::ThreadQuery` | `awaken::server_contract::storage::ThreadQuery` |
| `awaken_contract::contract::storage::ThreadRunStore` | `awaken_server_contract::contract::storage::ThreadRunStore` |
| `awaken_contract::contract::config_store::ConfigStore` | `awaken_server_contract::contract::config_store::ConfigStore` |
| `awaken_contract::contract::mailbox::MailboxStore` | `awaken_server_contract::contract::mailbox::MailboxStore` |
| `awaken_contract::contract::audit_log::AuditLogStore` | `awaken_server_contract::contract::audit_log::AuditLogStore` |
| `awaken_contract::contract::transport::Transcoder` | `awaken_server_contract::contract::transport::Transcoder` |

`awaken::prelude::*` continues to target common agent-building code. It does
not import every storage, commit, backend, or server-administration symbol from
0.5. Low-level implementors should import from `awaken_runtime_contract`,
`awaken_server_contract`, `awaken_runtime`, or `awaken_server` directly.

## Run Activation

`RunRequest` is replaced by `RunActivation`. The old flat request shape is now
split across user intent, input, options, tracing, runtime controls, capture
wiring, persistence hints, and inherited resolver state.

| 0.5 concept | 0.6 location |
|---|---|
| request thread/agent/kind | `RunActivation.intent` (`RunIntent`) |
| request messages | `RunActivation.input` (`RunInput::NewMessages` or `AlreadyPersisted`) |
| inference overrides and frontend tools | `RunActivation.options` |
| origin, run mode, adapter trace, parent thread/run | `RunActivation.trace` |
| cancellation, decisions, inbox, pending boundary | `RunActivation.control` |
| thread context cache | `RunActivation.capture` |
| run/dispatch identity hints and idempotency flags | `RunActivation.persistence` |
| pinned registry/resolver for replayable sub-runs | `RunActivation.inherited` |

Most callers should still construct runs with
`RunActivation::new(thread_id, messages).with_agent_id(agent_id)`. Server and
mailbox integrations use the lower-level fields to preserve dispatch ids,
resume HITL waits, and avoid re-persisting messages that were already appended.

## Models and Failover

The 0.5 model-binding API is replaced by the unified `ModelSpec` surface.
Builder registration now uses `with_model(spec)`, with the model id coming from
`spec.id`. Validation helpers, unknown-field policy constants, and mock provider
helpers use the same model-spec naming.

| 0.5 concept | 0.6 surface |
|---|---|
| provider/upstream model binding | `ModelSpec` |
| runtime provider/upstream pair | `ModelSpec` |
| model binding validation | `validate_model_spec` |
| model binding unknown-field policy | `MODEL_SPEC_UNKNOWN_FIELD_POLICY` |
| mock provider binding helper | `MockProviderProfile::model_spec()` |
| `fallback_upstream_models` | `ModelPoolSpec` with ordered pool members |

The HTTP/config wire key for model offerings remains `models`; old persisted
config documents that omit capability/pricing fields still parse. Model
failover now belongs in `ModelPoolSpec`, not provider-level
`fallback_upstream_models`.

## Resolution and Backends

The execution resolver/backend boundary is now typed around `Resolver`,
`ResolutionRequest`, `ResolvedRunPlan`, `ExecutionPlan`, and
`BackendProfile`.

| 0.5 API | 0.6 API |
|---|---|
| `ResolvedExecution` | `ExecutionPlan` / `ResolvedRunPlan` |
| ad-hoc resolver request state | `ResolutionRequest` |
| `BackendCapabilities` | `BackendProfile` |
| backend capability booleans | typed dimensions such as `DecisionCapability`, `PersistenceCapability`, `TranscriptCapability`, and `OutputCapability` |

`AgentResolver::resolve_execution(&agent_id)` remains for delegate/tool
resolution compatibility, but root execution uses the async `Resolver` trait:

```rust
async fn resolve(&self, request: ResolutionRequest) -> Result<ResolvedRunPlan, ResolveError>;
```

`ExecutionBackend::capabilities()` now returns `BackendProfile`. Backend
requests also carry additional runtime/server wiring such as `commit`,
`pending_boundary`, `state_seed`, and `thread_state`. Parent-to-child
`state_seed` is accepted only for local execution plans; non-local backends
reject seeded delegate requests instead of silently dropping the seed.

## Checkpoint Rename

`Checkpoint` is retained only as a deprecated type-name alias for
`ThreadCommit`. It does not preserve 0.5 struct literal fields or field access.

| 0.5 field | 0.6 field |
|---|---|
| `messages` | `message_delta` |
| `expected_message_version` | `expected_message_count` |
| `run` | `run_projection` |
| `thread_state` | `thread_state_snapshot` |

Deprecated constructor names still exist when they map cleanly:

| 0.5 helper | 0.6 helper |
|---|---|
| `Checkpoint::append(...)` | `ThreadCommit::append_messages(...)` |
| `Checkpoint::checkpoint_only(...)` | `ThreadCommit::run_projection_only(...)` |
| `checkpoint.with_thread_state(...)` | `thread_commit.with_thread_state_snapshot(...)` |

Code that used `Checkpoint { ... }` literals or read `checkpoint.messages`,
`checkpoint.run`, or `checkpoint.thread_state` must update to the new field
names.

## Commit Outcomes

`CheckpointCommitOutcome` is retained only as a deprecated type-name alias for
`ThreadCommitOutcome`. The runtime outcome no longer carries server event or
outbox ids.

| 0.5 outcome field | 0.6 location |
|---|---|
| `canonical_event_ids` | `awaken_server_contract::ThreadCommitStagedOutcome::canonical_event_ids` |
| `server_event_ids` | `awaken_server_contract::ThreadCommitStagedOutcome::server_event_ids` |
| `additional_outbox_ids` | `awaken_server_contract::ThreadCommitStagedOutcome::additional_outbox_ids` |

Use `CommitCoordinator::commit_checkpoint` for the runtime-only durability
boundary. Store implementations that need event/outbox ids should implement
`awaken_server_contract::StagedCommitCoordinator::commit_checkpoint_staged`,
which returns `ThreadCommitStagedOutcome`.

## Persistence Wiring

Runtime checkpoint writes now flow through a `CommitCoordinator`. The 0.5
builder/runtime store setters that accepted a `ThreadRunStore` directly are not
the durable write boundary in 0.6.

| 0.5 wiring | 0.6 wiring |
|---|---|
| `AgentRuntimeBuilder::with_thread_run_store(store)` | `AgentRuntimeBuilder::with_commit_coordinator(coordinator)` |
| `AgentRuntime::with_thread_run_store(store)` | `AgentRuntime::with_commit_coordinator(coordinator)` |
| `AgentRuntimeBuilder::thread_run_store()` | `AgentRuntimeBuilder::commit_coordinator()` / coordinator `reader()` |
| `AgentRuntime::thread_run_store()` | runtime checkpoint read port from the coordinator |
| `AgentRuntimeBuilder::with_mailbox_store(store)` | server `Mailbox` plus `MailboxStore` wiring |
| direct `ThreadRunStore` checkpoint writes | `MemoryCommitCoordinator`, `FileCommitCoordinator`, or `PgCommitCoordinator` |

`CommitCoordinator::reader()` supplies the runtime checkpoint read port, so the
runtime reads from the same store that the coordinator commits to. File-backed
coordinators are for dev/local deployments and require
`AWAKEN_ALLOW_DEV_FILE_COORDINATOR=true` in release builds; use
`PgCommitCoordinator` for strict transactional commits across thread/run,
event, and outbox writes.

## Server Embedding API

`awaken-server` now centers embedding around `ServerState`. `AppState` remains a
deprecated alias, so new code and docs should import `ServerState`.

| 0.5 API | 0.6 API |
|---|---|
| `AppState` | `ServerState` |
| route builders taking owned app state in older examples | `build_router(&ServerState)` or `build_service_router(ServerState)` |
| public field access on app state internals | module accessors such as `run_routes_state()`, `config_routes_state()`, and `admin_api_config()` |
| `AdminApiConfig { bearer_token, cors_allowed_origins, expose_config_routes }` | also configure `expose_trace_routes` and `expose_eval_routes` |
| `ServerConfig` without eval caps | `ServerConfig { eval_limits, .. }` |

Admin startup validation is stricter: if config, trace, or eval admin surfaces
are exposed, the server requires an admin bearer token from
`AdminApiConfig.bearer_token` or `AWAKEN_ADMIN_API_BEARER_TOKEN`. Setting
`expose_config_routes = false` only hides config/admin-run routes; it is not a
blanket "disable every admin feature" switch. Eval routes default to exposed,
trace routes default to hidden because traces can contain prompts and tool
arguments.

## Managed Config and Admin Behavior

Managed config publication covers agents, models, model pools, providers, MCP
servers, skills, plugin sections, and permission rules. Successful create,
update, delete, or override writes validate the candidate registry and publish
a snapshot for later runs.

Restore is intentionally different from normal config writes. A restore copies
the selected audit snapshot back into the editing `ConfigStore` and records a
fresh audit event, but it does not hot-swap the runtime registry. Review the
restored payload, then perform a normal config save when it should become the
active registry snapshot.

New server/admin route surfaces in 0.6 include system module discovery,
admin-authenticated run summaries, canonical thread/run event list and stream
routes, trace routes, eval dataset/run/online routes, provider removal preview,
and agent override validation. See the HTTP API reference for the full route
tree and exposure flags.

## Mailbox and Protocol Semantics

Mailbox dispatch status describes delivery lifecycle, not business success.
`Acked` means the dispatch was accepted/consumed; inspect the associated run
status, termination reason, and canonical events for execution outcome.

Protocol adapters now rely on the same scope, mailbox, cursor, and event-store
semantics as the server routes. AI SDK numeric `Last-Event-ID` values are
limited to the live replay buffer; durable canonical event resume uses the
opaque event cursors returned by `/v1/threads/:id/events` or
`/v1/runs/:id/events`. MCP HTTP sessions are mailbox-backed for run delivery and
cancellation.

## Cursor Behavior

Thread list cursors are now bound to the query shape that produced them.

Bare numeric cursors from 0.5 remain accepted only for unfiltered thread
listings. Filtered listings must continue with the opaque `next_cursor` returned
for the same query. A numeric cursor used with resource, parent/root, or backend
scope filters now fails with `cursor does not match thread query filters`.

`ThreadQuery.id_prefix` is backend-internal. Scoped store wrappers use it to
push tenant/scope filtering into the backend before pagination. HTTP routes do
not expose it as a user-controlled query parameter.

## Release Checks

Before publishing 0.6.0, compare the current public surface against 0.5.0:

```bash
cargo semver-checks check-release --baseline-version 0.5.0 -p awaken-contract
cargo semver-checks check-release --baseline-version 0.5.0 -p awaken
cargo semver-checks check-release --baseline-version 0.5.0 -p awaken-runtime
cargo semver-checks check-release --baseline-version 0.5.0 -p awaken-server
cargo semver-checks check-release --baseline-version 0.5.0 -p awaken-stores
```

`awaken-runtime-contract` and `awaken-server-contract` are new crate names for
0.6, so the compatibility check that matters is whether the old crates and the
`awaken` facade expose the intended migration surface.
