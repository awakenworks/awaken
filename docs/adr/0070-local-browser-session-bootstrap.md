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
