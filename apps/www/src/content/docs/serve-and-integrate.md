---
title: "Serve & Integrate"
description: "Build-time guidance for wrapping an AgentRuntime with server, protocol, mailbox, config, and admin surfaces."
---

Serve & Integrate is the last development step before
[Tune & Operate](/awaken/operate/): it turns a local runtime into something
other systems can call. Do this after [State & Storage](/awaken/state-and-storage/)
is wired, because server mode depends on durable stores for mailbox, config,
events, trace, eval, and recovery.

The value is one agent implementation behind many clients: server mode owns
the wire, queue, config, trace/eval, and admin surfaces while the runtime
remains the execution core.

## Runtime development vs server development

Runtime development uses Awaken as an in-process Rust library. Your application
owns the transport, request queue, auth, config loading, and operator workflow.
You build an `AgentRuntime`, register executable capabilities in code, and
decide how to feed `RunActivation` values into it. This mode still requires a
standard async Rust environment with Tokio available; it is not a `no_std` or
Tokio-free embedded-device target.

Server development keeps the same runtime execution core but lets
`awaken-server` own the service boundary around it. The server adds:

- HTTP resources for threads, runs, config, capabilities, and health.
- SSE streaming and protocol adapters for AI SDK v6, AG-UI, A2A, MCP, and ACP.
- Mailbox-backed background dispatch for resumable, cancellable, interruptible,
  and HITL-gated runs.
- Managed config APIs under `/v1/config/*` that validate, persist, compile, and
  publish registry snapshots.
- Admin-console workflows for editing agents/models/providers/plugin sections,
  previewing behavior, restoring config versions, and inspecting audit data.
- Server/store scope boundaries, protocol replay, outbox/event publication, and
  storage-backed run recovery.

Online config creates usable agents by publishing `AgentSpec`, `ModelSpec`,
provider settings, plugin sections, MCP servers, skills, and permission rules.
Executable code still has to provide the tools, plugins, providers, stores, and
backend factories those records reference.

## What changes when you serve it

Serving Awaken does not create a second agent implementation. The server wraps
the same `AgentRuntime` you can run in-process:

1. Protocol adapters parse client messages into `RunActivation`.
2. The mailbox stores and dispatches work so runs can be resumed, cancelled,
   interrupted, or recovered.
3. Runtime events are transcoded into the caller's protocol stream: AI SDK v6,
   AG-UI, A2A, MCP HTTP, or ACP stdio.
4. Admin routes mutate `/v1/config/*`; successful create/update/delete writes
   compile a validated registry snapshot and hot-swap it for later runs.

That gives you one backend for local Rust callers, browser chat clients,
operator tooling, and agent-to-agent integration. Tools and plugins still live
in code; prompts, models, provider wiring, permission rules, MCP servers, and
agent profiles move into managed config.

## Server module wiring

`ServerState` is assembled from modules. The route tree only exposes surfaces
whose module and exposure flag are present.

| Module | Adds | Typical wiring |
|---|---|---|
| Run | `/v1/threads`, `/v1/runs`, health | `AgentRuntime`, `Mailbox`, `ThreadRunStore`, resolver |
| Protocol | AI SDK v6, AG-UI, A2A, MCP HTTP | Same run module plus protocol adapters |
| Config | `/v1/config/*`, `/v1/capabilities`, audit, provider/MCP admin | `ConfigStore`, `ConfigRuntimeManager`, optional `AuditLogStore` |
| Events | `/v1/threads/:id/events`, `/v1/runs/:id/events` | `EventStore` plus server staged commits |
| Eval | `/v1/eval/*` | Config module, eval stores/services, `ServerConfig.eval_limits` |
| Trace | `/v1/traces*` | Trace store and `AdminApiConfig.expose_trace_routes = true` |

`AdminApiConfig.expose_config_routes`, `expose_eval_routes`, and
`expose_trace_routes` control the admin surfaces independently. If any of those
surfaces is exposed, startup requires an admin bearer token.

Scope is resolved at the server boundary through `HttpScopeProvider`.
`SingleScopeProvider::default_scope()` is the OSS/local default. Multi-tenant
deployments should derive `ScopeContext` from authenticated request state, let
server scoped stores apply backend filters, and expose the resolved `scope_id`
only as read-only UI context.

## Code references

Use these references when wiring a host application:

- `crates/awaken-doctest/examples/http_app_builder.rs` -- offline example for
  `AgentRuntime` → `Mailbox` → `ServerState`.
- `crates/awaken-server/src/app.rs` -- `ServerState` builders for config,
  trace, event, eval, admin, runtime stats, scope, and A2A push relay modules.
- `crates/awaken-server/src/app/modules.rs` -- module-specific state structs
  and the route surfaces they enable.
- `crates/awaken-server/tests/http_api.rs` and
  `crates/awaken-server/tests/transport_tests.rs` -- route and transport smoke
  coverage for served runs.

## Start here

1. Confirm [State & Storage](/awaken/state-and-storage/) choices for thread/run data, config, mailbox, events, trace, eval, and profile/shared state.
2. [Expose HTTP SSE](/awaken/how-to/expose-http-sse/) to put the runtime behind HTTP and streaming endpoints.
3. [Integrate AI SDK Frontend](/awaken/how-to/integrate-ai-sdk-frontend/) for React clients that speak AI SDK v6.
4. [Integrate CopilotKit (AG-UI)](/awaken/how-to/integrate-copilotkit-ag-ui/) for CopilotKit frontends.
5. [Use the Admin Console](/awaken/how-to/use-admin-console/) when operators should tune agents through the browser.
6. [Deploy to Production](/awaken/how-to/deploy-to-production/) to harden the server: durable stores, TLS, secrets, and health probes.

## Reference pages to pair with this section

- [HTTP API](/awaken/reference/http-api/)
- [AI SDK v6 Protocol](/awaken/reference/protocols/ai-sdk-v6/)
- [AG-UI Protocol](/awaken/reference/protocols/ag-ui/)
- [A2A Protocol](/awaken/reference/protocols/a2a/)
- [MCP HTTP Protocol](/awaken/reference/protocols/mcp/)
- [ACP Protocol](/awaken/reference/protocols/acp/)
