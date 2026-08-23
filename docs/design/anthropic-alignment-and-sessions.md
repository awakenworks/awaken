# Managed Agents Runtime Protocol Adapter

This doc covers the downstream `managed-agents-runtime-protocol` adapter. It is
intentionally not the runtime architecture and it does not include Managed Agents
management APIs. The concrete Dream management API and its Dream
orchestration are owned separately by
[Managed Dream](managed-dream.md).

## Protocol Naming

The public compatibility surface should be named
`managed-agents-runtime-protocol` when it exposes only runtime/session behavior:

- session or thread event ingress;
- run/message submission;
- streaming public runtime events;
- interruption, cancellation, and resume;
- custom/client-executed tool use.

The name must not imply support for agent, model, provider, credential, catalog,
or deployment management. Those capabilities remain config/admin concerns and
require separate contracts if they are ever exposed.

AG-UI is a separate public protocol adapter. It may share neutral runtime ports,
tool descriptors, wait/resume mechanics, event records, and
projection infrastructure, but it must not share Managed Agents DTOs, event
names, beta headers, error schemas, or conformance tests. Public protocol names
stay one adapter at a time; shared behavior lives below them as neutral runtime
and dispatch ports.

## Ownership

| Concern | Owner |
|---|---|
| Public DTOs, beta headers, error schema, event names | Product adapter |
| Runtime facts, `StreamEvent`, `EventRecord`, termination records | Runtime Core |
| HTTP/SSE routing, protocol replay, durable ingress | Dispatch / Server |
| Anthropic Outcome mapping | Product adapter over `awaken-ext-goal` |

## Anti-Corruption Layer

The adapter translates at the edge:

```text
public request/event/status <-> neutral runtime activation/fact/verdict
```

Public names such as `requires_action`, Managed Agents sessions, Anthropic beta
headers, or outcome result enums must not appear in runtime crates. Runtime errors
remain neutral; product adapters decide public error `type` and response shape.

## Sessions

Treat public sessions as product projections over runtime/server state:

- public session id maps to runtime thread/run identity through the adapter;
- identity is not authority;
- resumption behavior is a public protocol contract, not a runtime rule;
- public stream history is derived from committed facts/protocol replay.

Do not add a product `SessionRecord` to runtime core unless the same value object
is required for non-product runtime execution.

## Managed model and coordinator compatibility

The public model object is translated through the existing provider-neutral
`InferenceOptions` value. `effort`, `speed`, and `inference_geo` are frozen in
the published Agent revision and copied into each Session/thread snapshot. Geo
is a placement constraint, not route identity: an execution adapter must either
prove it can honor the exact value or reject the request before provider network
I/O. A coordinator and all ordinary roster agents must have identical explicit
geo pins, or all omit the pin. This validation belongs to the canonical Agent
publication path, not to a Cloud-only shadow registry.

The Managed compatibility boundary exposes only Anthropic's official
`inference_geo` vocabulary, `us | global`; `global` and omission both mean no
additional caller-selected placement constraint. Provider-specific or
non-Anthropic boundaries must not be added as fields to `model`, Session, or
Agent response DTOs. A hosting adapter may carry an opaque provider placement
proof in the existing resolved candidate and exact-target seams, but clients
cannot select its mechanism or observe Provider routing configuration through
the Managed wire. An internal non-US placement therefore projects as no public
`inference_geo`, while `us` remains portable: every Provider candidate must
prove US processing, and only Anthropic Messages may realize that proof by body
injection.

The versioned `agent_toolset_20260401` has exactly eight built-in names: `bash`,
`read`, `write`, `edit`, `glob`, `grep`, `web_fetch`, and `web_search`.
`web_search` reuses the one WebSearch plugin/provider registry and its existing
credential, egress, usage, and Billing seams; the Managed adapter only selects
it through the official toolset policy.

Multiagent authoring and execution continue through the one `AgentConfig` roster
and Session thread/event authority. The Managed roster admits only Agent
references and the official self reference. Internal advisor targets remain a
native control-plane capability and are neither accepted nor projected as a
private Managed union member.

Public Session status is restricted to the official
`rescheduling | running | idle | terminated` union. Provisioning and internal
failure phases remain internal state and are projected to the nearest official
observable state/event rather than leaking adapter-private enum members.

## MCP URL compatibility and sandbox stdio

The Managed wire accepts the official URL MCP shape. `sandbox_stdio` already has
one authoritative internal path—Agent binding, Session attachment/generation,
Environment process realization, and the runtime stdio MCP client—but a command
and arguments cannot be represented as a Managed URL. The Managed DTO therefore
has no stdio variant; native bindings are omitted rather than assigned a
fabricated URL.

The standards-preserving target design is:

| Classification | Authority | Role |
|---|---|---|
| Reuse unchanged | `McpTarget::SandboxStdio`, Session MCP generation/lease, Environment process realization, `awaken-ext-mcp` stdio client | Own the command, process, connection, and exact Session lifetime |
| Modify | Runtime Host `McpRelay` exact-generation capability table | Add a stdio upstream target beside its existing HTTP upstream target; keep one route/lease/cleanup authority |
| Modify | Managed Session MCP projection | Project the realized, capability-bearing bridge URL as the ordinary `{type:"url"}` shape |
| New | stdio request/notification adapter inside the relay | Translate Streamable HTTP requests and session lifecycle to the existing stdio MCP connection without building a second tool registry |
| New | native registration command, if callers must author commands | Register a sandbox command outside the Managed wire and return an opaque MCP reference; credentials remain typed bindings, never URL data |

Static structure:

```text
native Agent MCP binding (sandbox command)
  -> Session MCP attachment + exact generation/lease
  -> Session Environment process + existing stdio MCP connection
  -> existing McpRelay route table (stdio upstream variant)
  -> session-scoped capability URL
  -> standard Managed {type:"url", name, url} projection
```

Dynamic behavior:

```text
Session realization starts the frozen command
  -> initialize the stdio MCP peer
  -> atomically stage the exact-generation relay route
  -> expose the URL only after both process and route are healthy
  -> forward POST/GET/DELETE and notifications through that connection
  -> reject unknown, stale, expired, or cross-Session capabilities
  -> retry by creating a new generation and URL, never by retargeting an old one
  -> terminal Session cleanup removes the route and reaps the process
```

Until that bridge exists, native execution may consume sandbox stdio directly.
It must not fabricate a URL, expose command data through Managed fields, or
silently reinterpret a URL request as a local process.

## Events

Runtime emits neutral facts/events. The product adapter projects them:

- after commit;
- with public names and public status mapping;
- with product-specific exactly-once or replay rules;
- with no mutation of runtime truth.

Mid-run waits are represented by the neutral lifecycle/commit path and translated
to the product's public wait shape at the adapter.

## Custom Tool Use

Managed Agents custom tool use is a runtime wait/resume channel, not a
management API. The compatible slice maps a client-executed tool call through the
adapter:

```text
neutral tool request
  -> agent.custom_tool_use { id, name, input, session_thread_id? }
  -> user.custom_tool_result { custom_tool_use_id, content, is_error? }
  -> neutral tool result / run resume
```

The public `id` is the correlation and Session Thread routing key. It maps to the
neutral pending tool call id used to resume execution. Public `name` and `input` map to the selected
tool descriptor and arguments. Capability, deadline, fingerprint, permission, and
policy metadata are derived from resolved config and runtime context; they are
not accepted from the public event and are not projected back onto the public
wire.

Frontend or client-executed tools reuse this same public channel. Do not add
adapter-specific `agent.frontend_tool_use` or `user.frontend_tool_result` event
types. The distinction between custom, frontend, remote, or builtin tool sources
belongs to descriptor/catalog metadata before projection, not to runtime core.

Inbound `user.custom_tool_result` must fail closed when the referenced pending
tool call is unknown, already answered, expired, mismatched by thread/session, or
not owned by a client-executed tool. Successful results and error results are
both normalized into the same neutral result path before the runtime continues.
For multiagent replies, the qualified public tool-use event id is the routing
authority; the SDK input does not accept a separate Session Thread selector.
Outbound events may echo `session_thread_id`, but clients reply with the event
id. Legacy unqualified ids are accepted only when the committed pending call and
reply kind identify exactly one Session Thread. Validation carries that one
resolved target through activity admission and execution rather than resolving
again or keeping a protocol-side tool registry.

This support does not imply support for Managed Agents management endpoints.
Clients may provide per-run client tool descriptors when an adapter explicitly
allows that runtime option, but they may not create, update, publish, or delete
agent specs, model/provider bindings, credentials, or global tool catalog
entries through the custom-tool channel.

## Outcome / Goal Mapping

Anthropic Outcome is a product specialization:

| Product concept | Neutral mapping |
|---|---|
| `user.define_outcome` | command adapted to `outcome::Definition` and the Outcome Runtime Extension |
| outcome evaluation span | projection of durable Outcome Evaluation records |
| `satisfied`, `needs_revision`, etc. | product mapping of `outcome::EvaluationResult` |
| max iterations | Outcome extension policy plus the Runtime Step ceiling as a runaway backstop |

The Outcome Extension owns Iterations, Worker/Grader Runs, and recovery over
durable Worker Thread state. Runtime Core records neutral Run facts; the product
adapter owns public outcome names, wire compatibility, and external-runtime
input projection. Neither Runtime Core nor ACP learns Outcome vocabulary.

## First Vertical Slice

For a Managed Agents-compatible product slice:

1. define one public request DTO;
2. map it to one neutral runtime activation or Outcome extension command;
3. execute through server/runtime ports;
4. project one committed fact to one public event;
5. add snapshot tests proving no public names leak into runtime errors.

## Non-Goals

- No Anthropic DTOs in runtime crates.
- No product session model as runtime truth.
- No public event emitted before the runtime commit.
- No broad Managed Agents product crate family beyond concrete adapter slices;
  Dream is one such product slice and retains its own owner document.

## Guardrails

G1, G10, and G11 in [INVARIANTS](../INVARIANTS.md).
