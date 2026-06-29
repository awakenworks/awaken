# Protocol Adapter Boundaries

This document makes public protocol adapters explicit. A protocol adapter owns a
wire contract and anti-corruption mapping. It does not own runtime execution,
config authoring, admin operations, or credential lifecycle.

## Supported Public Surfaces

| Public surface | Adapter status | Shared runtime substrate | Must stay separate |
|---|---|---|---|
| `managed-agents-runtime-protocol` | runtime/session/custom-tool compatibility | `RunActivation`, `RunIngress`, committed facts, wait/resume | Managed Agents management endpoints, config CRUD, admin tools |
| AG-UI | UI-oriented run/event protocol | `RunActivation`, committed event projection, client-executed tools | Managed Agents DTOs, beta headers, event names |
| AI SDK / CopilotKit-style HTTP | model/run client compatibility | submit/cancel/stream through runtime ports | provider selection or runtime-internal errors |
| Internal/native HTTP | server-owned runtime API | neutral submit/control/query ports | product-specific public status names |

Adapters may share mapping helpers only below the public DTO layer. Public event
names, request schemas, error schemas, headers, replay ids, and conformance
fixtures remain one adapter at a time.

## Adapter Contract

Every public adapter must name the following mapping before implementation:

| Mapping area | Adapter owns | Runtime-facing result |
|---|---|---|
| Request decode | public DTOs, headers, beta flags, route auth | rejected request or neutral command |
| Run submit | public run/session/thread ids and input shape | `RunActivation` plus optional `RunIngress` delivery mode |
| Resume/control | public cancel, interrupt, decision, custom-tool result | `RunIngress` or `LiveRunControl` command |
| Client-executed tools | public descriptor/result shape | per-run client tool descriptors or tool result |
| Stream output | public SSE/event framing | projection from live stream or committed records |
| Replay/history | public replay cursor and idempotency contract | committed facts/events plus protocol replay rows |
| Error mapping | public error code and message | neutral error classified by source domain |
| Unsupported API | explicit rejection for management/product endpoints | no runtime call |

The adapter boundary is crossed only by neutral runtime values. Public DTO structs,
public event enums, route state, auth claims, tenant objects, beta-header flags,
and public error types must not enter runtime core.

## Runtime Flow

```text
public request
  -> adapter auth and DTO validation
  -> neutral submit/control/resume command
  -> RunIngress / LiveRunControl
  -> runtime execution and commit
  -> committed facts/events
  -> adapter projection
  -> public stream, replay, or response
```

Live stream delivery may be used for latency, but committed facts remain the
source of replayable public history. If a public event describes a durable wait,
tool result, message, terminal state, or outcome, it must be reconstructable from
committed runtime truth or protocol replay rows written with the same commit.

## Runtime-Only Managed Agents Compatibility

`managed-agents-runtime-protocol` means:

- session/thread runtime event ingress;
- run or message submission;
- SSE/history projection;
- cancel, interrupt, decision, and resume;
- `agent.custom_tool_use` / `user.custom_tool_result`;
- optional `agent.tool_use` / `user.tool_result` for a self-hosted runner slice.

It does not mean:

- agent definition CRUD;
- model/provider/tool catalog mutation;
- credential, vault, workspace, or account APIs;
- admin assistant tools;
- publish/version-switch/draft workflows;
- product billing, quota, or deployment management.

Unsupported management endpoints should fail before runtime ports are called.

## Conformance

Each public adapter needs adapter-owned conformance fixtures:

1. request-to-activation mapping;
2. committed facts to public event projection;
3. replay from committed facts and protocol replay cursor;
4. public error mapping for at least one runtime, config, permission, dispatch,
   and environment failure;
5. negative tests proving unsupported management endpoints do not call runtime or
   config write ports;
6. leak tests proving public DTO vocabulary does not appear in runtime contracts.

## First Vertical Slice

For a new adapter, implement only this first slice:

1. one submit endpoint that produces `RunActivation`;
2. one stream projection from committed events;
3. cancellation through `RunIngress` or `LiveRunControl`;
4. one wait/resume fixture if the protocol supports client tools;
5. conformance snapshots for request, event, replay, and error mapping.

## Guardrails

G10, G19, and G26 in [INVARIANTS](../INVARIANTS.md).
