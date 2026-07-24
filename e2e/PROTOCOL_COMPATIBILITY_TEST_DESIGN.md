# Managed Agents / AI SDK / AG-UI / A2A compatibility test design

## 1. Objective and oracle

The compatibility target is not four independent implementations. Awaken has one
neutral runtime contract and four protocol projections:

```text
provider / ACP / MCP / A2A execution
              │
              ▼
      neutral Fact + Delta stream
              │
       fold / lifecycle / retry
              │
   ┌──────────┼──────────┬──────────┐
   ▼          ▼          ▼          ▼
Managed    AI SDK      AG-UI       A2A
SDK wire   SSE/JSON    SSE/JSON    Task/JSON-RPC
```

The neutral facts are the behavioral oracle. Protocol tests must never compare
implementation-specific event ids, timestamps, SSE chunk sizes, or field ordering.
They compare normalized facts, terminal state, durable history, and error semantics.
Official SDKs are wire oracles only: `@anthropic-ai/sdk`, `@ag-ui/client`, and
`@a2a-js/sdk` validate public decoding and request shapes. They do not replace
neutral-runtime tests.

## 2. Static and dynamic views

### Static ownership

| Boundary | Authoritative owner | Contract under test | Reuse rule |
|---|---|---|---|
| Run lifecycle, retries, permission, persistence | neutral agent/session contracts | `Fact`, `StepOutcome`, session lifecycle | test once with a fact fixture |
| Managed Agents | `awaken-protocol-managed` | beta headers, DTOs, event catalog, cursor/SSE | SDK contract and wire golden |
| AI SDK adapter | `awaken-protocol-ai-sdk` / bridge | thread/run/message projection | normalize to facts |
| AG-UI adapter | `awaken-protocol-ag-ui` | run lifecycle, message/tool deltas | normalize to facts |
| A2A adapter | `awaken-protocol-a2a` | Agent Card, Task, JSON-RPC, streaming | normalize Task to facts |
| Provider/ACP/MCP fixtures | scenario host and testkits | deterministic causes and faults | one fixture per cause, many projections |

### Dynamic lifecycle

```text
create/bind → queued → running →
  message ───────────────┐
  tool → allow/deny/result ┤
  retry → rescheduled ────┤
  interrupt/steer ────────┤
  failure → error ────────┤
                         ▼
                  idle / awaiting / terminated
                         │
              replay, list, stream, cross-wire read
```

For every transition, assert both the live response and the durable projection.
For reconnect tests, assert `normalize(list ∪ replay(stream))` is lossless and
deduplicated by stable event/task/message identity.

## 3. Cause-effect graph

The test generator uses these causes and effects. A row is covered only when every
listed effect is observed on at least one wire and in the neutral fact test.

| Cause | Partitions / boundaries | Required effects |
|---|---|---|
| plain user message | empty, Unicode, large, multi-turn | user→assistant, ordering, idle |
| server tool | allow, ask-allow, ask-deny, result error | tool use/result, awaiting, resume, terminal |
| MCP tool | allow, always_ask, auth failure, connection failure | MCP-specific events, confirmation, classified error |
| custom/client tool | client result, error result, timeout | custom tool events and continuation |
| provider fault | 4xx terminal, 429 exhausted, 5xx retry once, retry exhaustion | error type, retry status, rescheduled, session reusable |
| lifecycle race | concurrent sends, interrupt running, archive/delete | serialization, cancellation, conflict fences |
| persistence fault | restart before/after commit, stale lease, replay | exactly-once, lease reclaim, no phantom events |
| resource policy | read-only/read-write, egress deny/limited, file downloadable flag | enforcement, safe error, immutable source |
| protocol framing | JSON, SSE named/data-only, chunk split/merge, malformed frame | decoder tolerance without semantic drift |
| cross-protocol ingress | A2A→AI SDK→AG-UI→Managed | same neutral history and independent wire snapshots |

## 4. Test layers and required properties

1. **Neutral property tests**: feed generated `Fact` sequences to every transcoder;
   assert exhaustive handling, no dropped terminal fact, and equivalent normalized
   semantics. Include tool ids crossing step boundaries (the MCP regression case).
2. **Wire contract tests**: official SDK request/response decoding, beta headers,
   DTO discriminators, pagination, SSE framing, JSON-RPC error envelopes, Agent Card.
3. **State-machine E2E**: deterministic scenario host drives each cause row through
   each legal transition and checks durable history.
4. **Fault/recovery E2E**: kill/restart, lease expiry, retry, reconnect and concurrent
   operations; assert invariants rather than timing.
5. **Cross-protocol E2E**: one thread id and one neutral history, with each adapter
   acting as ingress and egress at least once.
6. **Live-provider lanes**: DeepSeek/KIMI/Anthropic are smoke evidence only; failures
   are classified as transport, authentication, quota, model behavior, or projection.

## 5. Compatibility matrix

Each cell is a test family, not a single example. `N` means neutral fixture first;
`M`, `S`, `G`, and `A` mean Managed, AI SDK, AG-UI, and A2A projections.

| Family | N | M | S | G | A | Current evidence / missing design |
|---|---:|---:|---:|---:|---:|---|
| plain turn + replay | ✓ | ✓ | ✓ | ✓ | ✓ | existing suites; add normalized parity assertion |
| tool allow/result | ✓ | ✓ | ✓ | ✓ | ✓ | existing fake-provider suites |
| tool ask allow/deny | ✓ | ✓ | ✓ | partial | partial | add shared `hitl_matrix` fixture |
| MCP allow/ask | ✓ | ✓ | partial | partial | partial | Managed allow + ask now covered; add cross-wire adapters |
| MCP auth/connection errors | ✓ | ✓ | partial | partial | partial | `management_mcp_e2e.mjs` covers 401/unreachable; cross-wire error projection remains |
| retry/reschedule/exhaustion | ✓ | ✓ | partial | partial | partial | expand projections and retry-status normalizer |
| active interrupt/steer | ✓ | ✓ | missing | missing | missing | shared delayed-run fixture |
| restart/reconnect/lease | ✓ | ✓ | partial | partial | partial | Managed route + SQLite durable queue now verify `reclaim_older_than_ms`; cross-wire replay remains |
| read-only memory/files | ✓ | ✓ | partial | partial | partial | `managed_memory_extraction_durable_e2e.mjs` proves fail-closed write/extraction; add cross-wire ingress |
| limited networking subflags | ✓ | ✓ | partial | partial | partial | `management_egress_bind_e2e.mjs` covers neutral policy; package/MCP subflags remain |
| files negative/content blocks | partial | partial | missing | missing | missing | multipart + document/image matrix |
| deployment/task lifecycle | ✓ | ✓ | missing | missing | ✓ | add adapter task snapshot checks |
| cross-protocol continuity | ✓ | ✓ | ✓ | ✓ | ✓ | `cross_protocol_a2a_continuity_e2e.mjs`; extend both directions |

## 6. Concrete missing suites to add

These are the remaining executable designs, ordered by contract risk:

- `protocol_neutral_transcoder_properties.rs`: generated facts, every adapter,
  terminal-preservation and MCP result identity across resume steps.
- `e2e/protocol_hitl_matrix_e2e.mjs`: same delayed built-in/custom/MCP call, allow and
  deny, through Managed, AI SDK, AG-UI and A2A; compare normalized outcome.
- `e2e/managed_mcp_error_e2e.mjs`: unreachable endpoint and 401 fixture; assert
  `session.error` classification, server name, retry status, and session usability.
- `e2e/protocol_recovery_matrix_e2e.mjs`: 503 once, 503 exhausted, restart before
  commit, reconnect, and lease reclaim across all adapters.
- `e2e/protocol_resource_policy_e2e.mjs`: read-only write denial, downloadable=false,
  egress `{deny,limited}` with `allow_mcp_servers` and `allow_package_managers` true/false.
- `e2e/protocol_files_content_matrix_e2e.mjs`: multipart upload/download negatives and
  text/document/image `file_id` blocks through each ingress.
- `e2e/protocol_cross_wire_roundtrip_e2e.mjs`: each of the four protocols writes one
  turn and every other protocol reads it; task snapshots remain immutable where A2A
  requires snapshots.

## 7. Exit criteria and evidence

The goal is complete only when:

1. every cause row has a neutral test and at least one official-SDK wire test;
2. every adapter has inbound, outbound, streaming, error, and replay coverage;
3. normalized fact traces are equal for equivalent scenarios;
4. durable history survives restart and `list ∪ stream` has no loss/duplication;
5. all negative cases assert status, stable error class, retryability, and session state;
6. `cargo test --workspace`, deterministic E2E, SDK/AG-UI/A2A conformance, and live
   smoke results are reported separately; quota/network failures never count as model
   compatibility passes;
7. the matrix is regenerated from test manifests so a new neutral fact or SDK event
   cannot silently lack a projection test.

Ark is an optional provider lane only. It cannot be the sole fixture for local
persistence, sandbox, lease, security, deployment, or fault-injection causes.
