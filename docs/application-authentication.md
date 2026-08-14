# Application authentication

Awaken keeps one authoritative conversation chain:

```text
trusted backend / Apply
  -> create or resolve Managed Session
  -> mint short-lived token with external thread -> Managed Session binding
browser AI SDK or AG-UI
  -> application guard resolves that exact binding
  -> existing Managed Session / runtime thread
  -> Agent sandbox and tool output
  -> design_submit_artifact -> Design Collector -> Artifact publisher
  -> immutable child Revision -> Studio SSE refresh
```

There is no hash-derived `app_*` Session, application mapping table, chat proxy,
or browser Artifact write path. The Managed Session repository is the resource
authority; an application token is only an ephemeral capability projection.

## Static authority and permissions

The customer's backend remains authoritative for users, roles, memberships,
projects, and which application-visible thread belongs to which Managed Session.
Awaken does not import that policy model.

| Owner | Authoritative data or decision |
|---|---|
| Customer/Design backend | end-user authorization, `chat_thread_id`, and its durable `managed_session_id` link |
| Managed Sessions | Session existence, Workspace owner, frozen Agent baseline, lifecycle, transcript, and sandbox |
| Application token issuer | validates existing Sessions and projects a short-lived binding |
| Application guard | exact route, protocol, operation, external-thread binding, and bound Agent enforcement |
| AI SDK / AG-UI adapters | protocol translation only; both use the resolved Managed Session |

The process composition keeps two authentication domains disjoint. Session and
token-management routes use the service/IAM edge; AI SDK and AG-UI routes are
mounted after that edge because they already carry the application guard. Both
credentials use the standard `Authorization: Bearer` header, so stacking the
two guards would reinterpret one credential as two unrelated authorities and
make every valid request fail. This separation is routing composition only: it
does not add a proxy, token type, identity store, or alternate protocol path.

An application grant contains:

- opaque `authority_id`, `application_scope`, and optional `actor_key` for
  correlation—not runtime identity derivation;
- allowed `protocols`: `ai-sdk` and/or `ag-ui`;
- allowed `operations`: `thread.run` and/or `thread.messages.read`;
- one-to-one `thread_bindings` from an external thread id to an existing Managed
  Session id. The bound Agent is copied from that Session's frozen baseline and
  cannot be supplied as a second authority by the caller.

The token does **not** grant Managed Agents API access and does not expose an
Awaken-wide conversation list. `thread.messages.read` permits only
`GET /v1/{protocol}/threads/{bound-external-id}/messages` for an explicitly
bound thread. A Design product should list conversations from its own durable
`chat_thread_id -> managed_session_id` links, then mint a narrow token for the
selected links. A service API key remains required for `/v1/sessions` list and
management operations.

## Backend exchange

After applying its own authorization policy, the trusted backend first creates
or resolves the canonical Managed Session:

```http
POST /v1/sessions
Authorization: Bearer <service-api-key>
Content-Type: application/json

{"agent":"support","title":"Design chat"}
```

It then mints a short-lived application token for the selected protocol,
operation, and binding:

```http
POST /v1/application-access-tokens
Authorization: Bearer <service-api-key>
Content-Type: application/json

{
  "authority_id": "my-backend",
  "application_scope": "project_42",
  "actor_key": "opaque-user-ref",
  "protocols": ["ai-sdk"],
  "operations": ["thread.run", "thread.messages.read"],
  "thread_bindings": [{
    "external_thread_id": "chat_thread_7",
    "managed_session_id": "sesn_123"
  }],
  "expires_in_seconds": 300
}
```

The issuer rejects missing or foreign-workspace Sessions, unfrozen baselines,
and terminal Sessions when `thread.run` is requested. Bindings must be complete,
unique, and one-to-one. Tokens expire after at most 15 minutes, can be revoked
with `DELETE /v1/application-access-tokens/{id}`, and are invalidated by an
Awaken process restart.

## Frontend AI SDK

Return the external thread id and only `access_token`/`expires_at` to the browser:

```ts
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";

const { access_token, thread_id } = await fetch("/api/awaken-token")
  .then((response) => response.json());

const chat = useChat({
  id: thread_id,
  transport: new DefaultChatTransport({
    api: `${AWAKEN_URL}/v1/ai-sdk/threads/${thread_id}/runs`,
    headers: { Authorization: `Bearer ${access_token}` },
  }),
});
```

Keep the application token in memory. Never put a service API key in browser
source, browser storage, a mobile bundle, or a public environment variable.

## Dynamic enforcement and failures

For every application request, the guard authenticates the token, classifies an
exact public route with a default-deny table, verifies its protocol and
operation, and resolves the presented external thread id through the token's
binding. It then attaches the existing Managed Session and frozen Agent through
the shared resolved-resource seams before dispatching to the protocol adapter.

The trusted backend reaches Session lifecycle and application-token issuance
through the service/IAM guard first. The resulting browser request reaches only
the application guard. If an application protocol is accidentally mounted
inside the service/IAM edge, process authorization fails closed rather than
falling back to service credentials or accepting a dual-purpose bearer.

Missing/expired/revoked tokens return `401`. Unknown routes, ungranted protocols
or operations, unbound threads, and Agent overrides return `403`; conflicting
path/body thread ids return `400`. None of these failures creates a Session or
falls back to hashing. Retries with a replacement token must carry the same
explicit binding if they are to continue the same conversation.

In explicit local `NoLogin` mode, management and service APIs retain the local
single-user trust posture. Application protocol routes still require an
application token; the local management surface is the issuer.
