# ADR-0023: Admin API Surface and Exposure Policy

- **Status**: Accepted
- **Date**: 2026-04-26
- **Depends on**: ADR-0010, ADR-0018

## Context

The server hosts an admin/configuration HTTP surface — `/v1/config/:namespace`,
`/v1/config/:namespace/:id`, `/v1/config/:namespace/$schema`, `/v1/agents`,
`/v1/agents/:id`, `/v1/capabilities`. These endpoints write to the
`ConfigStore` (PUT, POST, DELETE) and read every namespace including raw
provider documents.

Until now the access policy lived implicitly across three places:

1. `AdminApiConfig::bearer_token` — optional bearer required by every
   handler via `ensure_admin_auth`.
2. `validate_admin_surface` — refuses to start the server when binding a
   non-loopback address without a bearer token, *if* a config store or
   runtime manager is attached.
3. `build_router` — unconditionally merges `config_routes()`.

This ad-hoc structure has two problems:

- **There is no way to opt out of the routes entirely.** Embedders that
  drive configuration through their own RBAC + audit pipeline can either
  set a bearer token (still serves the routes, just behind one shared
  secret) or set a loopback bind (excludes legitimate non-loopback uses).
  Neither matches "do not expose this surface at all."
- **The policy is split across three files** with no single source of
  truth, making it easy for new admin endpoints to drift from the
  established convention.

## Decisions

### D1: AdminApiConfig is the single container for admin policy

All admin/configuration access policy lives in `AdminApiConfig`:

```rust
pub struct AdminApiConfig {
    pub bearer_token: Option<String>,
    pub cors_allowed_origins: Vec<String>,
    pub expose_config_routes: bool, // default: true
}
```

Future admin policy fields (rate limits, audit hooks, additional
authentication strategies) extend this struct rather than introducing
parallel mechanisms.

### D2: `expose_config_routes` opt-out

When `expose_config_routes` is `false`, `build_router` does not merge
`config_routes()` into the public router. The endpoints return `404 Not
Found` regardless of the request method or bearer header — the routes are
not registered at all, so there is no handler to gate.

Default is `true` for back-compat. Embedders that own their own
configuration plane set it to `false` via:

- `AppState::with_admin_api_config(...)` builder, or
- `AWAKEN_ADMIN_EXPOSE_CONFIG_ROUTES` environment variable (when wired
  through the embedder; the environment override is not provided by the
  framework today and may be added when there is a use case).

### D3: `validate_admin_surface` reads `expose_config_routes` first

`validate_admin_surface` short-circuits to `Ok` when
`expose_config_routes` is `false`. The bearer-token-on-non-loopback
requirement applies only when the routes are actually mounted. This
preserves the security posture for the default case while letting
embedders that own configuration through other channels run on any bind
address without a server-side bearer token.

### D4: New admin endpoints share the toggle

Future admin endpoints (e.g., audit log, runtime control) mount alongside
`config_routes()` and respect the same `expose_config_routes` toggle. If a
future endpoint warrants independent exposure control, that is a
deliberate ADR amendment, not an ad-hoc field addition.

### D5: The toggle controls mounting, not authentication

`expose_config_routes = true` does not imply "open access" — bearer
authentication via `bearer_token` and the non-loopback validation in
`validate_admin_surface` continue to apply. The toggle is an additional
upstream gate, not a replacement.

## Consequences

- Embedders running their own RBAC / audit can run on any bind address
  with `expose_config_routes = false` and no admin bearer token, without
  the framework refusing to start.
- The default surface is unchanged: defaults remain
  `expose_config_routes = true` and `bearer_token = None`, with the
  loopback / bearer requirement enforced at startup when a config store
  or runtime manager is attached.
- `build_router` now requires `&AppState` so it can read the effective
  admin policy at compose time. Test code constructs an `AppState` and
  passes it; the API change is mechanical.
- `validate_admin_surface` is shorter and reads top-down by relevance:
  not exposed → token configured → no admin attached → loopback → fail.
