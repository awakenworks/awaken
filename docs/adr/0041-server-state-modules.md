# ADR-0041: `ServerState` Modules — Replacing the `AppState` Service Locator

- **Status**: Accepted
- **Date**: 2026-05-22
- **Depends on**: ADR-0038, ADR-0039, ADR-0040
- **Updates**: ADR-0023 server composition, ADR-0034 server event wiring
- **Breaking**: yes (0.6.0, server crate)

## Context

`AppState` carries required runtime state plus many optional dependencies.
A second registry, `APP_STATE_EXTRAS`, stores services that did not fit the
public struct shape: admin config, audit config, runtime stats, audit log,
trace store, event store, eval store, construction time, and credential
broker. Handlers discover these dependencies through accessors at runtime,
not through axum state types.

This makes route availability and handler dependencies implicit. Tests also
need global extra-state setup before exercising handlers that rely on those
services.

## Decision

Replace `AppState` with typed `ServerState` modules and route states. Required
verticals (`run`, `protocol`, `system`) are mounted unconditionally; optional
verticals are mounted only when their module exists. Handlers extract concrete
route/module state, never `Option<...>` and never the whole `ServerState`.

### D1: Module decomposition and field ownership

```rust
pub struct ServerState {
    pub run: RunModuleState,
    pub config: Option<ConfigModuleState>,
    pub events: Option<EventModuleState>,
    pub eval: Option<EvalModuleState>,
    pub trace: Option<TraceModuleState>,
    pub protocol: ProtocolModuleState,
    pub admin: AdminModuleState,
    pub server_config: ServerConfig,
    pub scope_provider: Arc<dyn HttpScopeProvider>,
}

#[derive(Clone)]
pub struct RunModuleState {
    pub runtime: Arc<AgentRuntime>,
    pub mailbox: Arc<Mailbox>,
    pub resolver: Arc<dyn AgentResolver>,
    /// Single thread/run store handle. Reads happen directly; writes go
    /// through ADR-0038's `CommitCoordinator`, so the coordinator boundary
    /// — not a parallel read-only trait — is what keeps modules from
    /// performing ad hoc durable writes.
    pub store: Arc<dyn ThreadRunStore>,
    pub credential_broker: Arc<dyn CredentialBroker>,
    pub runtime_stats: Option<Arc<RuntimeStatsRegistry>>,
}

#[derive(Clone)]
pub struct ConfigModuleState {
    pub config_store: Arc<dyn ConfigStore>,
    pub runtime_manager: Arc<ConfigRuntimeManager>,
    pub audit_log: Option<Arc<AuditLogger>>,
    pub skill_catalog_provider: Option<Arc<dyn SkillCatalogProvider>>,
}

#[derive(Clone)]
pub struct EventModuleState {
    /// Single canonical event handle. `EventStore` is itself the trait
    /// composition `EventWriter + EventReader + EventLookup + EventSubscriber`,
    /// so handlers that need any of those surfaces extract them through
    /// this one field. Protocol replay logs, outbox publishers, and
    /// fanout publishers are constructed independently against the same
    /// underlying store and wired where they are used (mailbox capture,
    /// protocol projectors); they do not flow through this module state.
    pub event_store: Arc<dyn EventStore>,
}

#[derive(Clone)]
pub struct EvalModuleState {
    pub eval_run_store: Arc<dyn EvalRunStore>,
}

#[derive(Clone)]
pub struct TraceModuleState {
    pub trace_store: Arc<dyn TraceStore>,
}

#[derive(Clone)]
pub struct ProtocolModuleState {
    pub replay_buffers: ReplayBufferMap,
    pub mcp_http: Arc<McpHttpState>,
    pub a2a_push_outbox: Arc<dyn OutboxStore>,
    pub a2a_push_relay_config: A2aPushWebhookRelayConfig,
}

#[derive(Clone)]
pub struct AdminModuleState {
    pub admin_api_config: AdminApiConfig,
    pub audit_log_config: AuditLogConfig,
    pub started_at: Instant,
}
```

`ServerState::new` requires a mailbox. Callers that do not provide an
external mailbox backend use `ServerState::new_with_local_mailbox`, which
installs an in-memory mailbox so run-control and protocol routes keep the same
HTTP surface. This local mailbox is process-local and best-effort; durable and
multi-replica deployments provide an externally backed `Mailbox`.

`ProtocolModuleState` and `scope_provider` are mandatory. The default protocol
module installs in-memory replay buffers, MCP HTTP state, and an in-memory A2A
push webhook outbox; deployments that need durable push delivery replace the
outbox through the protocol setters. The default scope provider is a single
scope.

Default protocol wiring also attaches an in-memory A2A push webhook outbox to
the active `ProtocolModuleState` replay buffers. This preserves the public A2A
capability surface for single-process servers. Deployments that require retry
durability across restart or replicas replace it with a durable `OutboxStore`
via `with_a2a_push_webhook_relay`.

`APP_STATE_EXTRAS` is deleted. Its fields move as follows:

| Existing source | New owner |
|---|---|
| `admin_api_config`, `audit_log_config`, `started_at` | `AdminModuleState` |
| `runtime_stats`, `credential_broker` | `RunModuleState` |
| `audit_log`, `skill_catalog_provider` | `ConfigModuleState` |
| `trace_store` | `TraceModuleState` |
| `event_store` (composed reader/writer/lookup/subscriber) | `EventModuleState` |
| `eval_run_store` | `EvalModuleState` |
| `replay_buffers`, `mcp_http`, A2A push webhook outbox | `ProtocolModuleState` |

### D2: Router composition with concrete module state

Module route builders are typed over route-specific state. `ServerState`
constructs those route states, and each `RouteModule` mounts itself onto the
top-level router:

```rust
pub(crate) trait RouteModule {
    fn mount(self, router: Router) -> Router;
}

impl<M: RouteModule> RouteModule for Option<M> {
    fn mount(self, router: Router) -> Router {
        match self {
            Some(module) => module.mount(router),
            None => router,
        }
    }
}

pub fn build_router(state: &ServerState) -> Router {
    let mut router = Router::new();
    router = state.run_routes_state().mount(router);
    router = state.protocol_routes_state().mount(router);
    router = SystemRoutes(state.system_routes_state()).mount(router);
    router = state.event_module().mount(router);
    router = state.config_routes_state().mount(router);
    router = state.eval_routes_state().mount(router);
    router = state.trace_routes_state().mount(router);
    router
}
```

Run, protocol, and system routes are mounted unconditionally. Optional module
routes mount through `Option<M: RouteModule>` and become no-ops when their
module state is absent or the admin exposure flag excludes them.

A route that needs multiple modules gets a route state at the composition
root:

```rust
#[derive(Clone)]
pub struct ConfigRoutesState {
    pub admin: AdminModuleState,
    pub run: RunModuleState,
    pub config: ConfigModuleState,
    pub scope_provider: Arc<dyn HttpScopeProvider>,
}

pub fn config_routes() -> Router<ConfigRoutesState> { /* ... */ }

impl RouteModule for ConfigRoutesState {
    fn mount(self, router: Router) -> Router {
        router.merge(config_routes().with_state(self))
    }
}
```

This is the required axum pattern: no `Router<S>` with optional state is
nested into the top-level router.

### D3: Cross-module operation rule

Combined states are allowed for one- or two-module handlers. If an operation
needs three or more modules, or if a vertical requires another module to do
real work, the composition root builds a purpose-specific service and mounts
a router whose handlers extract that service state. Module states must not
embed other module states; otherwise top-level modules can drift and point at
different `Arc` instances.

```rust
#[derive(Clone)]
pub struct EvalExecutionService {
    pub run: RunModuleState,
    pub eval: EvalModuleState,
    pub config: ConfigModuleState,
    pub trace: Option<TraceModuleState>,
    pub events: Option<EventModuleState>,
}

pub fn eval_execution_routes() -> Router<EvalExecutionService> { /* ... */ }

if let (Some(eval), Some(config)) = (state.eval.clone(), state.config.clone()) {
    let service = EvalExecutionService {
        run: state.run.clone(),
        eval,
        config,
        trace: state.trace.clone(),
        events: state.events.clone(),
    };
    router = router.nest("/v1/eval/run", eval_execution_routes().with_state(service));
}
```

Handlers do not assemble tuples from `ServerState` and do not extract
`ServerState`. The required dependency set remains visible at the route
boundary.

### D4: Mandatory and optional route behavior

Run, protocol, and system routes are always mounted because their state is
mandatory. If an optional module is absent, its routes are absent and axum
returns 404. This is a 0.6.0 API behavior change from handlers that were
always mounted and returned 503 for missing optional stores. Admin exposure
flags are still checked for admin-gated modules, but route mounting requires
both:

1. the exposure flag permits the surface; and
2. the module state is present.

A configured module with a disabled exposure flag is not mounted. An enabled
exposure flag with no module is also not mounted.

Client upgrade behavior is explicit:

| Absent module | Route families that are absent and return 404 |
|---|---|
| `config` | `/v1/config/*`, config-backed agent/tool/model/provider admin routes, config-backed eval execution routes |
| `events` | `/v1/threads/:id/events*`, `/v1/runs/:id/events*` |
| `trace` | `/v1/traces*` |
| `eval` | `/v1/eval/*` |

`/v1/system/modules` is mounted unconditionally and lists mounted modules,
including mandatory `run`, `admin`, and `protocol`. Clients that previously
probed optional handlers by expecting 503 should switch to that endpoint or
deployment configuration.

### D5: `AppState` is a deprecated compatibility alias only

0.6.0 requires new code to construct `ServerState` explicitly. The public
`AppState` name remains only as a `#[deprecated]` type alias to avoid a
needless source break for route tests and callers that use the type name.
Legacy `with_*` setters remain as compatibility helpers, but each setter
updates typed module state instead of restoring the old service-locator model.

```rust
impl ServerState {
    pub fn new(run: RunModuleState, server_config: ServerConfig) -> Self { /* ... */ }
    pub fn with_config(mut self, cfg: ConfigModuleState) -> Self { /* ... */ }
    pub fn with_events(mut self, events: EventModuleState) -> Self { /* ... */ }
    pub fn with_eval(mut self, eval: EvalModuleState) -> Self { /* ... */ }
    pub fn with_trace(mut self, trace: TraceModuleState) -> Self { /* ... */ }
    pub fn with_protocol(mut self, protocol: ProtocolModuleState) -> Self { /* ... */ }
    pub fn with_scope_provider(mut self, provider: Arc<dyn HttpScopeProvider>) -> Self { /* ... */ }
    pub fn with_config_store(self, store: Arc<dyn ConfigStore>) -> Self { /* compatibility */ }
    pub fn with_event_store(self, store: Arc<dyn EventStore>) -> Self { /* compatibility */ }
    pub fn with_trace_store(self, store: Arc<dyn TraceStore>) -> Self { /* compatibility */ }
}
```

## Migration

| 0.5.x | 0.6.0 |
|---|---|
| `AppState::new(...).with_config_store(cs)` | compatibility setter builds `ConfigModuleState`; new code may call `with_config(...)` directly |
| `state.event_store()` returning `Option` | handlers extract `State<EventModuleState>` |
| `APP_STATE_EXTRAS` | explicit fields on module states |
| optional routes returning 503 | module-gated routes returning 404 when absent |
| handler extracts `State<AppState>` | handler extracts route state, module state, or purpose-specific service state |

## Risks

- Tests must construct relevant module states explicitly.
- Clients that used 503 from absent optional handlers as capability probing
  must switch to route discovery or documented deployment configuration.
- Cross-module services need names and ownership at route boundaries, not
  ad hoc extraction inside handlers.

## Test Plan

1. A `ServerState` with only required state mounts run, protocol, and system
   routes.
2. A state with `events` mounts event routes whose handlers receive
   `EventModuleState` directly.
3. `APP_STATE_EXTRAS` has no matches in server code.
4. A two-module route compiles with a combined state pre-bound through
   `.with_state(combined)` before nesting.
5. Eval/protocol module states do not embed config/event module states;
   cross-module eval/protocol operations use purpose-specific service state.
6. A three-module operation is represented by a purpose-specific service
   state, not `ServerState` extraction.
7. Absent optional modules return 404 for the documented route families and
   appear absent from `/v1/system/modules`; mandatory modules are always listed.

## Non-Goals

- Splitting `ServerConfig` into per-module configs.
- Multi-tenant module isolation.
