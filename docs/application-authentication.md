# Application authentication

Awaken separates credentials by caller:

```text
trusted backend                           browser / mobile application
      |                                              |
      | service API key                              | application token
      v                                              v
Managed Agents API                    AI SDK / AG-UI protocol endpoints
      |                                              |
      +---------------- Awaken runtime ---------------+
```

The customer's backend remains the authority for users, roles, memberships,
projects, and policy. Awaken does not import that model. It accepts only:

- `application_scope`: an opaque project, tenant, installation, or other
  application boundary;
- `thread_namespace`: an opaque namespace within that scope;
- `actor_key`: an optional opaque audit correlation value;
- allowed operations: `thread.run` and/or `thread.read`;
- an Agent allow-list and optional default Agent.

The tuple of workspace, issuing authority, application scope, thread namespace,
and external thread id resolves to a stable internal thread id. The same external
id in another scope cannot read or continue that thread.

## Backend token exchange

Keep the workspace-scoped service API key on the backend. After applying the
customer's own authorization policy, request a short-lived application token:

```http
POST /v1/application-access-tokens
Authorization: Bearer <service-api-key>
Content-Type: application/json

{
  "authority_id": "my-backend",
  "application_scope": "project_42",
  "thread_namespace": "customer-chat",
  "actor_key": "opaque-user-ref",
  "operations": ["thread.run", "thread.read"],
  "agent_ids": ["support"],
  "default_agent_id": "support",
  "expires_in_seconds": 300
}
```

Return only `access_token` and `expires_at` to the application. Phase-one tokens
expire after at most 15 minutes, can be revoked with
`DELETE /v1/application-access-tokens/{id}`, and are invalidated by an Awaken
process restart. A backend may mint a replacement with the same scope without
changing the thread mapping.

## Frontend AI SDK

Use the application token with the official Vercel AI SDK transport:

```ts
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";

const token = await fetch("/api/awaken-token").then((response) => response.json());
const threadId = crypto.randomUUID();

const chat = useChat({
  id: threadId,
  transport: new DefaultChatTransport({
    api: `${AWAKEN_URL}/v1/ai-sdk/threads/${threadId}/runs`,
    headers: { Authorization: `Bearer ${token.access_token}` },
  }),
});
```

Keep the application token in memory. Do not put a service API key in browser
source, browser storage, a mobile bundle, or a public environment variable.

## Backend Managed Agents API

Trusted services call the Managed Agents API directly with the service API key:

```http
POST /v1/sessions
Authorization: Bearer <service-api-key>
Content-Type: application/json

{"agent":"support","title":"backend job"}
```

When embedded or cloud IAM is enabled, Managed Agents and A2A execution routes
require this service credential. AI SDK and AG-UI routes instead require an
application token. MCP uses its separately configured bearer.

In explicit local `NoLogin` mode, management and service APIs retain the local
single-user trust posture. Application protocol routes still require an
application token; the local management surface is the issuer.
