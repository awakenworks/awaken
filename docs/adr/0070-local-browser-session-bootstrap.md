# ADR-0070: Local browser session bootstrap

- Status: Accepted
- Date: 2026-07-28
- Depends on: ADR-0048, ADR-0061, awaken-iam ADR-0012
- Supersedes: ADR-0061 identity-mode default and browser management-bearer flow

## Context

The `no-login` local default made the console immediately usable but left local
HTTP management routes unauthenticated. The self-managed path instead exposed a
long-lived bootstrap API token for operators and the browser stored that token
in `localStorage`. That duplicated credential purpose: API tokens are for CLI
and automation, while awaken-iam already owns opaque browser sessions.

## Decision

Local deployments default to `self-managed`. `no-login` remains an explicit
development choice and `awaken-cloud` remains the hosted identity choice.

Static ownership is:

```text
awaken CLI                 awaken-control              awaken-iam
prints one-time handoff -> product route mounting -> challenge + SessionGateway
                            product role binding       cookie principal
                            route/scope PEP        ->  shared IamGate/PDP
```

The stable `local-console-admin` Account receives the same Org-scoped management
and resource roles as local browser administration requires. It does not replace
the bootstrap service principal or its API token, which remain automation and
recovery credentials. The console no longer reads or writes
`awaken.console.token`; local requests use an HttpOnly cookie.

The dynamic flow is:

1. Startup opens the existing embedded IAM and creates a five-minute setup
   challenge through awaken-iam.
2. CLI prints the cleartext token once.
3. The console probes canonical `GET /v1/session`; a 401 shows the setup form,
   while a 404 means the deployment does not expose local session auth.
4. Same-origin `POST /v1/auth/local/exchange` consumes the token once and sets
   the HttpOnly session cookie.
5. `management_guard` resolves the cookie through the existing `IamGate`, then
   applies its existing workspace fence, action mapping, PDP decision, audit,
   and handler sequence.
6. Invalid, expired, replayed, or foreign-origin setup fails closed. Logout
   revokes the session through canonical `DELETE /v1/session`.

## Consequences

- Default local startup is authenticated without asking users to configure an
  identity provider or paste a reusable bearer into Settings.
- Browser and API-token principals share one authorization policy; no parallel
  product session store or PDP is introduced.
- Restart currently invalidates the in-memory browser session and issues a new
  setup window. Session durability, if required, must replace awaken-iam's
  session directory implementation rather than add a product-owned path.
- Consumers pin the awaken-iam revision containing ADR-0012 as one atomic
  cross-repository dependency update.

## Amendment — 2026-08-10: retain authorization across restart

The in-memory restart consequence above is superseded. Embedded IAM already
owns the migrated `<data_dir>/iam.sqlite` `SessionRepo`; Awaken now passes
that exact repository to awaken-iam's canonical `SessionGateway`. No
product-owned session table, cookie parser, hydration cache, or alternate
authorization path is added.

The restart sequence is:

1. Awaken reopens and migrates the existing `iam.sqlite`.
2. `LocalBrowserAuth` creates a fresh five-minute setup challenge for browsers
   that do not yet have a session and attaches a repository-backed Gateway to
   the existing `IamGate`.
3. A browser presenting its prior HttpOnly cookie is resolved by token hash
   from `iam_sessions`; expiry and persisted revocation are checked before
   the existing PDP and Workspace fence run.
4. A missing, expired, or revoked row returns 401 and the browser uses the
   current one-time setup token. Repository failure returns 503 and never
   clears or admits the browser session.

The server-side session and browser cookie keep the existing 30-day bound.
`last_seen_at` is durable activity metadata, not sliding expiry. Logout writes
`revoked_at` to the same row, so restart cannot revive a logged-out browser.

## Amendment — 2026-08-23: Cloud-connected interactive default

The self-managed local default above is superseded for the interactive
`all-in-one` product. When neither identity nor model supply is explicitly
configured, `awaken` resolves the existing axes to
`identity_mode = "awaken-cloud"` and `cloud_models = "enabled"`, then uses
awaken-iam-client's canonical native authorization-code-with-PKCE adapter before
starting Cloud model discovery.

This is a product preset, not a new persisted mode. An explicit `no-login` or
`self-managed` identity keeps Cloud models disabled unless a valid compatible
combination is authored; an explicit `awaken-cloud` identity may still select
`cloud_models = "disabled"` for Cloud login plus Workspace-BYOK-only use.
Server and split-service roles do not start an interactive browser login and
continue to require explicit deployment identity and service credentials.

Static ownership remains singular:

```text
Awaken CLI       -> loopback/browser adapter + existing CredentialCache port
awaken-iam       -> OP discovery, provider selection, PKCE/code, refresh, UserInfo
Awaken Cloud     -> desktop-client registration, tenant/model/tool admission
Awaken runtime   -> exact published local or brokered model/tool capability
```

At startup a live cached token is reused; an expired token with refresh state is
rotated; otherwise the browser enters IAM's unified authorization endpoint.
Failure to bind the registered loopback address, authenticate, refresh, discover
models, or satisfy entitlement fails closed with a typed startup/readiness
error. Choosing an explicit local identity bypasses this network flow and reuses
the existing embedded/no-login behavior unchanged.
