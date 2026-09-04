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

## Application Authentication Role Catalog

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| Customer/Design backend | External authority | End-user authorization, `chat_thread_id`, and its durable `managed_session_id` link | Managed Session and application-token APIs | Awaken Session, Run, token, or protocol state | An unauthorized customer user obtains a valid application binding | Product authorization tests and the explicit backend exchange below |
| Managed Sessions | Durable aggregate | Session existence, Workspace owner, frozen Agent baseline, lifecycle, transcript, and sandbox | Session repository and Runtime applications | Customer thread mapping or application credential storage | A token binds a missing, foreign, unfrozen, or terminal Session | Coordinator issuance decision table and Session repository conformance |
| `issue_application_access` | Application service | Canonical request validation, exact Session binding projection, and one credential mint | Managed Session repository and `ApplicationAccessStore` | Caller product relations, HTTP response mapping, persistence, or token authentication | A product adapter repeats Session/token policy or calls the store primitive directly | Transport-neutral issuance and HTTP route decision tables |
| `ApplicationAccessStore` | Durable internal component | Token hash, expiry/revocation stamps, Workspace, and the short-lived application grant | Coordinator Session database, scoped migrations, and token entropy/hash primitives | Cleartext token recovery, IAM role bindings, Session metadata, or a process-local fallback | Replica or process restart loses mint/revoke truth, or an unavailable store is mistaken for invalid credentials | SQLite/PostgreSQL two-instance conformance, restart/revoke tests, and provisioned PostgreSQL release gate |
| `ApplicationAccessAuthenticator` | Boundary port | Authentication result consumed by the protocol PEP | The Coordinator-owned `ApplicationAccessStore` implementation | Minting, revocation, persistence, caching, or fallback | A protocol guard reaches a second credential authority | Public API snapshot and application-guard tests |
| Application guard | Permission gate | Exact route, protocol, operation, external-thread binding, Workspace, and bound-Agent enforcement | `ApplicationAccessAuthenticator` and the Managed Session binding in the grant | Session creation, token minting, protocol translation, or customer authorization | A bearer bypasses tenancy or dispatches before an authority failure | Default-deny route and tenancy decision tables |
| AI SDK / AG-UI adapters | Protocol adapters | Wire translation only | The resolved Managed Session and canonical Run application | Authentication, customer mapping, or an alternate Run path | Protocol handling diverges into a second execution authority | Protocol adapter and application-authentication E2E tests |

The process composition keeps two authentication domains disjoint. Session and
token-management routes use the service/IAM edge; AI SDK and AG-UI routes are
mounted after that edge because they already carry the application guard. Both
credentials use the standard `Authorization: Bearer` header, so stacking the
two guards would reinterpret one credential as two unrelated authorities and
make every valid request fail. This separation is routing composition only: it
does not add a proxy, token type, identity store, or alternate protocol path.

Caller-specific users, projects, and correlation identifiers remain in the
customer backend; the token API neither accepts nor echoes them. An application
grant contains only:

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
and terminal Sessions when `thread.run` is requested.
Bindings must be complete,
unique, one-to-one, limited to 32 per token, and each external or Managed
Session id is limited to 255 bytes. This bounds the two authoritative Session
reads required for each binding. Mint reuses the existing Managed Create
request limiter; it does not create an application-specific rate limiter.
Tokens expire after at most 15 minutes, can be revoked with
`DELETE /v1/application-access-tokens/{id}`, and remain valid across a
Coordinator restart until expiry or revocation. Revoke is fenced by the
authenticated Workspace: a missing, repeated, or foreign id returns the same
`204`, while a foreign Workspace cannot invalidate the owner's credential.
SQLite deployments keep the record in the configured Coordinator Session
database; PostgreSQL deployments use that same selected database so every
Coordinator replica observes mint and revocation without sticky routing. The
provisioned PostgreSQL release gate fails if this two-instance conformance does
not run. Only the one-way token hash is stored; the cleartext returned by the
mint response cannot be read back and the response carries
`Cache-Control: no-store`.

The Coordinator's existing supervised service lifecycle deletes expired or
revoked records after a 24-hour retention window. Once per minute it drains
batches of at most 512 rows, yielding after each full batch and stopping on a
short batch or after 256 batches. Live credentials and terminal credentials
still inside the window are retained. Cleanup failure stops that tick; the next
tick resumes from the same durable repository and never enables a process-local
authorization fallback. This is bounded best-effort maintenance, not a claim
that Open can cap the global backlog: Hosted admission is partitioned per
organization and Open does not bound the number of active organizations. For
durable backlog `Q`, new eligible rows `E`, and deletions `D`, each tick follows
`Q[n+1] = max(0, Q[n] + E[n] - D[n])`.

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

### Pre-uploaded Files

AI SDK `FileUIPart` is reserved for a hosted URL or a data URL. It is not
reinterpreted as an Awaken Files API id. A browser that has already uploaded a
File uses the typed custom data-part extension below instead:

```json
{
  "type": "data-awaken-file",
  "data": {
    "object": "awaken.file_reference",
    "fileRef": "file_0123456789abcdef0123456789abcdef",
    "kind": "document"
  }
}
```

The part and nested data object are closed. `fileRef` is exactly `file_` plus a
lowercase 32-hex UUID; `kind` is `image` or `document`. No URL, bytes, filename,
MIME type, credential, Workspace id, or provider id crosses this wire. The AI
SDK adapter maps the part to a neutral logical File source, and the
attempt-bound content materializer re-resolves its metadata and bytes in the
authenticated Workspace before provider I/O. The reference alone grants no
read authority. Committed history emits the same reference-only data part, so
reload does not manufacture a browser-download URL.

This is an AI SDK custom data part, not a second upload protocol or a Product
resource type. Unknown `data-*` parts remain non-content; malformed
`data-awaken-file` parts fail the whole request before Run admission instead of
being silently dropped or downgraded to text.

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

An unavailable or corrupt application-access repository returns one fixed `503`
before a protocol adapter can dispatch. Session-repository failures during mint
also use one fixed `503`; neither response exposes an adapter URL, SQL error, or
corrupt row detail. Repository failure is not collapsed into `401`, cached
locally, or retried against an in-memory fallback. Revocation is idempotent and
consistent across replicas within the authenticated Workspace: deleting an
already-revoked, unknown, or foreign valid token id returns the same terminal
success and never requires a read-before-delete.

In explicit local `NoLogin` mode, management and service APIs retain the local
single-user trust posture. Application protocol routes still require an
application token; the local management surface is the issuer.
