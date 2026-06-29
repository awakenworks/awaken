# Managed Agents Runtime Protocol Adapter

This doc covers the downstream `managed-agents-runtime-protocol` adapter. It is
intentionally not the runtime architecture and it does not include Managed Agents
management APIs.

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
  -> user.custom_tool_result { custom_tool_use_id, content, is_error?, session_thread_id? }
  -> neutral tool result / run resume
```

The public `id` is only a correlation key. It maps to the neutral pending tool
call id used to resume execution. Public `name` and `input` map to the selected
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

This support does not imply support for Managed Agents management endpoints.
Clients may provide per-run client tool descriptors when an adapter explicitly
allows that runtime option, but they may not create, update, publish, or delete
agent specs, model/provider bindings, credentials, or global tool catalog
entries through the custom-tool channel.

## Outcome / Goal Mapping

Anthropic Outcome is a product specialization:

| Product concept | Neutral mapping |
|---|---|
| `user.define_outcome` | `GoalSpec` / thread-scoped goal state |
| outcome evaluation span | continuation verdict fact / trace projection |
| `satisfied`, `needs_revision`, etc. | product mapping of opaque `GoalOutcome` detail |
| max iterations | goal extension policy and runtime backstop |

The runtime core records opaque verdicts and terminal conclusions. The product
adapter owns public outcome names and compatibility behavior.

## First Vertical Slice

For a Managed Agents-compatible product slice:

1. define one public request DTO;
2. map it to one neutral runtime activation or goal command;
3. execute through server/runtime ports;
4. project one committed fact to one public event;
5. add snapshot tests proving no public names leak into runtime errors.

## Non-Goals

- No Anthropic DTOs in runtime crates.
- No product session model as runtime truth.
- No public event emitted before the runtime commit.
- No broad Managed Agents product crate family unless a concrete adapter slice
  exists.

## Guardrails

G1, G10, and G11 in [INVARIANTS](../INVARIANTS.md).
