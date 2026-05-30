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

Replace `AppState` with typed `ServerState` modules. Each optional vertical
is represented by a module state, and routers for optional verticals are
mounted only when that module exists. Handlers extract concrete module
state, never `Option<...>` and never the whole `ServerState`.

### D1: Module decomposition and field ownership

```rust
pub struct ServerState {
    pub run: RunModuleState,
    pub config: Option<ConfigModuleState>,
    pub events: Option<EventModuleState>,
    pub eval: Option<EvalModuleState>,
    pub trace: Option<TraceModuleState>,
    pub protocol: Option<ProtocolModuleState>,
    pub admin: AdminModuleState,
    pub server_config: ServerConfig,
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
| `replay_buffers`, `mcp_http` | `ProtocolModuleState` |

### D2: Router composition with concrete module state

Module route builders are typed over their own state. They are converted to
`Router<()>` with `.with_state(...)` before being nested into the top-level
router:

```rust
pub fn run_routes() -> Router<RunModuleState> { /* handlers use State<RunModuleState> */ }
pub fn config_routes() -> Router<ConfigModuleState> { /* ... */ }
pub fn event_routes() -> Router<EventModuleState> { /* ... */ }

pub fn build_router(state: ServerState) -> Router {
    let mut router: Router = run_routes()
        .with_state(state.run.clone());

    if let Some(config) = state.config.clone() {
        router = router.nest("/v1/config", config_routes().with_state(config));
    }

    if let Some(events) = state.events.clone() {
        router = router.nest("/v1/events", event_routes().with_state(events));
    }

    if let Some(trace) = state.trace.clone() {
        router = router.nest("/v1/traces", trace_routes().with_state(trace));
    }

    if let Some(eval) = state.eval.clone() {
        router = router.nest("/v1/eval", eval_routes().with_state(eval));
    }

    if let Some(protocol) = state.protocol.clone() {
        router = router
            .nest("/v1/mcp", mcp_routes().with_state(protocol.clone()))
            .nest("/v1/a2a", a2a_routes().with_state(protocol.clone()))
            .nest("/v1/ai-sdk", ai_sdk_routes().with_state(protocol));
    }

    router
}
```

`eval_routes()` in this example covers eval-store surfaces that need only
`EvalModuleState`. Eval execution routes that also need run/config state
use the service-state pattern in D3.

A route that needs two modules gets a combined state at the composition
root:

```rust
#[derive(Clone)]
pub struct ConfigRunModuleState {
    pub run: RunModuleState,
    pub config: ConfigModuleState,
}

pub fn config_run_routes() -> Router<ConfigRunModuleState> { /* ... */ }

if let Some(config) = state.config.clone() {
    let combined = ConfigRunModuleState { run: state.run.clone(), config };
    router = router.nest(
        "/v1/config-run",
        config_run_routes().with_state(combined),
    );
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

### D4: Optional routes change absent-module behavior

If a module is absent, its routes are absent and axum returns 404. This is
a 0.6.0 API behavior change from handlers that were always mounted and
returned 503 for missing optional stores. Admin exposure flags are still
checked, but route mounting requires both:

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
| `protocol` | `/v1/mcp/*`, `/v1/a2a/*`, `/v1/ai-sdk/*` |

Deployments that need capability discovery expose an admin-protected
`GET /v1/system/modules` response listing which modules are mounted. Clients
that previously probed optional handlers by expecting 503 should switch to
that endpoint or deployment configuration.

### D5: `AppState` is a deprecated compatibility alias only

0.6.0 requires new code to construct `ServerState` explicitly. The public
`AppState` name remains only as a `#[deprecated]` type alias to avoid a
needless source break for route tests and callers that use the type name but
not the old service-locator setters. The alias does not preserve the old
`AppState::with_*` model; those setters are removed.

```rust
impl ServerState {
    pub fn new(run: RunModuleState, server_config: ServerConfig) -> Self { /* ... */ }
    pub fn with_config(mut self, cfg: ConfigModuleState) -> Self { /* ... */ }
    pub fn with_events(mut self, events: EventModuleState) -> Self { /* ... */ }
    pub fn with_eval(mut self, eval: EvalModuleState) -> Self { /* ... */ }
    pub fn with_trace(mut self, trace: TraceModuleState) -> Self { /* ... */ }
    pub fn with_protocol(mut self, protocol: ProtocolModuleState) -> Self { /* ... */ }
}
```

The old `AppState::with_config_store(...)`, `with_event_store(...)`,
`with_trace_store(...)`, and similar scattered setters are deleted.

## Migration

| 0.5.x | 0.6.0 |
|---|---|
| `AppState::new(...).with_config_store(cs)` | `ServerState::new(run, cfg).with_config(ConfigModuleState { config_store: cs, ... })` |
| `state.event_store()` returning `Option` | handlers extract `State<EventModuleState>` |
| `APP_STATE_EXTRAS` | explicit fields on module states |
| always-mounted optional routes returning 503 | module-gated routes returning 404 when absent |
| handler extracts `State<AppState>` | handler extracts one module state or purpose-specific service state |

## Risks

- Tests must construct relevant module states explicitly.
- Clients that used 503 from absent optional handlers as capability probing
  must switch to route discovery or documented deployment configuration.
- Cross-module services need names and ownership at route boundaries, not
  ad hoc extraction inside handlers.

## Test Plan

1. A `ServerState` with only `run` mounts only run-level routes.
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
   appear absent from `/v1/system/modules`.

## Non-Goals

- Splitting `ServerConfig` into per-module configs.
- Multi-tenant module isolation.
