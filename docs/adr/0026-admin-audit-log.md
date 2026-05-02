# ADR-0026: Admin Audit Log

- **Status**: 📐 Proposed
- **Date**: 2026-05-02
- **Depends on**: ADR-0023

## Context

Admin actions — config writes (`PUT`, `POST`, `DELETE` on
`/v1/config/:namespace/:id`), runtime control operations
(`/v1/mcp-servers/:id/restart`), and future state transitions such as publish
(see ADR-0025) — leave no structured record. The only signal available today is
the OpenTelemetry span emitted by `awaken-ext-observability` and the axum
request logs, neither of which captures the resource payload before and after
the change.

This gap matters in three scenarios:

1. **Incident investigation.** When a live agent misbehaves, operators need to
   know who changed the agent spec, when, and what the previous value was.
2. **Compliance.** Regulated environments require a tamper-evident log of every
   configuration change.
3. **Admin console UX.** The console has no history page. Users rely on memory
   or external version control to understand what changed.

The existing observability stack in `crates/awaken-ext-observability` emits
OTel metrics and traces but does not persist a queryable event log. Traces are
ephemeral and are not designed for structured querying by resource or actor.

## Decisions

### D1: Event shape

Each audit event is a self-contained JSON document:

```
{
  "id":         "<ulid>",
  "ts":         "<RFC 3339 timestamp>",
  "actor":      "<SHA-256 prefix of the bearer token, or 'anonymous'>",
  "action":     "<create | update | delete | publish | restart>",
  "resource":   "<namespace>/<id>",
  "before":     <JSON or null>,
  "after":      <JSON or null>,
  "ip":         "<client IP or null if behind a proxy without forwarding>",
  "request_id": "<X-Request-Id header value or null>"
}
```

`before` and `after` carry the full stored document at the point of change.
For `delete`, `after` is null. For `create`, `before` is null. For
`restart`, both are null (no document is modified).

The actor field uses a truncated hash of the bearer token rather than the token
itself, preserving the ability to correlate events from the same caller without
storing credentials.

### D2: Storage

Audit events are stored in the existing `ConfigStore` under a dedicated
namespace key `_audit`. The underscore prefix is reserved by `ConfigNamespace`
validation and is never exposed as a user-facing namespace, so there is no
collision risk.

This reuses `ConfigStore` — including its file, in-memory, and Postgres backends
in `crates/awaken-stores/` — without introducing a new trait or dependency.
The tradeoff is that audit events share the same storage backend as config
documents; teams that want an independent audit sink (e.g., write-once object
storage) can introduce an `AuditLogStore` trait at that time as a deliberate
extension point.

Entries are appended using ULID-based keys so time-ordering is preserved
without a secondary sort index.

### D3: Query API

```
GET /v1/audit-log
  ?since=<RFC 3339>          # lower bound on ts (inclusive)
  &until=<RFC 3339>          # upper bound on ts (exclusive)
  &action=<action>           # filter by action name
  &resource=<namespace/id>   # filter by exact resource path
  &actor=<hash prefix>       # filter by actor hash prefix
  &limit=<n>                 # max results, default 100, cap 1000
  &cursor=<opaque string>    # keyset cursor for pagination
```

Response:

```json
{
  "items": [ ...events... ],
  "next_cursor": "<string or null>"
}
```

The endpoint mounts inside `config_routes()` and is gated by the same
`ensure_admin_auth` check used by all other admin routes (ADR-0023, D4).

### D4: Retention

The server applies a rolling retention window. The default window is 90 days;
entries older than the window are pruned during a background sweep that runs
at server startup and on a configurable interval. The retention window is
configurable via `AdminApiConfig` (extending the struct introduced in ADR-0023,
D1). Deployments that require longer retention should configure an external
backup of the `_audit` namespace before the window expires.

### D5: Actor identification

The actor field in each event is derived as follows:

1. If the request carries a valid `Authorization: Bearer <token>` header, the
   actor is the first 16 hex characters of the SHA-256 hash of the token bytes.
2. If the server is configured without a bearer token (open admin), the actor
   is `"anonymous"`.
3. Clients may supply an `X-Awaken-Actor` header with a human-readable label
   (e.g., a CI job name). When present, this label is appended to the actor
   field: `"<hash>/<label>"`. The label is truncated to 64 bytes and must
   contain only printable ASCII; non-conforming values are dropped silently.

The `X-Awaken-Actor` header is advisory and unauthenticated; it assists
readability but is not a security boundary.

## Alternatives Considered

**Stream to OTel only.** Low implementation cost, but OTel backends are not
designed for structured querying by resource or actor, and the `before`/`after`
payload would bloat span attributes beyond practical limits. Rejected as the
sole mechanism; OTel spans may be emitted in addition to the structured log as
a diagnostic aid.

**Write to a local file.** Simple, but the file is not queryable via the admin
API and is not replicated across server instances. Rejected as the primary
store; a future sidecar or log-shipping agent may tail the file as a secondary
export path.

**Separate `AuditLogStore` trait with dedicated implementations.** Cleanest
architecture, but premature until there is evidence that audit storage
requirements diverge from `ConfigStore` semantics. The `_audit` namespace
approach can be migrated to a dedicated trait later without changing the API
surface.

## Consequences

- The admin console gains a foundation for an Audit Log page: fetch
  `GET /v1/audit-log` and render a time-ordered table. No new API design is
  required.
- Storage cost is proportional to the volume of admin actions and the
  configured retention window. Config namespaces are typically low-write; audit
  events are small JSON documents. The overhead is expected to be negligible for
  most deployments.
- `ConfigService` methods (`create`, `update`, `delete`) must emit audit events
  after a successful store write. This adds one async write per admin mutation
  on the critical path.
- Any SLO on admin endpoint latency must account for the additional store write.
  The write is non-transactional with the config write; if the audit write
  fails, the config change is already committed. Implementers should log the
  failure and continue rather than rolling back the config change.
