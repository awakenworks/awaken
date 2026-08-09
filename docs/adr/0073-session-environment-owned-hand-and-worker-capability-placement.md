# ADR-0073: Session Environment–Owned Hand and Worker Capability Placement

- Status: Accepted
- Date: 2026-08-01
- Supersedes: ADR-0046's per-Run `ToolExecutorProvider` placement and Agent
  `declared_hand`/deployment `hand_connections` configuration
- Amends: ADR-0044; its `ToolExecutor` and relay remain reusable mechanisms, but
  a production Session obtains a Hand only from its realized Environment
- Depends on: ADR-0065, ADR-0066, ADR-0071, ADR-0072

## Context

Ingress protocols are resident Coordinator adapters. They translate HTTP/SSE or
JSON-RPC into the same protocol-neutral Session/Run application ports; they are
not execution locations. A durable claim selects a Worker. That Worker realizes
the frozen Environment and executes the selected backend:

- Native: model loop plus Sandbox-target Hand tools;
- ACP: an opaque CLI process inside the same Environment;
- outbound A2A: remote network IO, with no local Hand or Environment.

The previous design also allowed a Host-global or per-Run Hand chosen from live
Agent configuration. This was a second placement authority beside durable Worker
placement and a second Hand owner beside SessionEnvironment. It could route
Native filesystem effects outside the Environment shared with ACP.

## Decision

### Static structure

| Bounded context | Owns | Depends on |
|---|---|---|
| Protocol adapters | external method/path, wire validation and projection | `RunApplication` from `awaken-session-contract` |
| Coordinator | Session/Run orchestration, durable dispatch, Worker eligibility and ranking | immutable Agent, Environment and Resource snapshots |
| Worker | claimed attempt, capability evidence and SessionEnvironment realization | provisioning/runtime contracts |
| SessionEnvironment | one live sandbox and its one `ToolExecutor` | Workdir, Namespace or Container provider |
| Native runtime | brain loop and target-based tool routing | Session `tool_executor` for Sandbox tools |
| ACP adapter | opaque process launch | the same SessionEnvironment |
| outbound A2A adapter | authenticated remote IO | no local SessionEnvironment |

`SandboxRequirements` is the one neutral demand vector. Coordinator derives it
from the frozen Environment and resource access. `SandboxCapabilities` uses the
same monotonic predicate both for provider admission and Worker claim admission.
Ranking can order only Workers that already satisfy this predicate.

There are no `brain worker` and `hand worker` domain types. A Worker is
capability-bearing: it may advertise Native, exact ACP profiles, and generic A2A
transport. Hand is a SessionEnvironment capability, not an independently placed
microservice. AllInOne always starts the same registered WorkerNode; local ACP
discovery only enriches that Worker's capabilities.

### Dynamic behavior

```text
external client
  -> resident protocol adapter
  -> Coordinator Session/Run application
  -> durable dispatch(Agent + Environment + Resources + requirements)
  -> eligible registered Worker claims
  -> Worker realizes one SessionEnvironment when local execution needs it
     -> Native: brain inference; Sandbox tool -> Environment ToolExecutor
     -> ACP: CLI process -> same Environment
     -> outbound A2A: remote HTTP IO; skip local Environment
  -> committed result / awaiting / failed terminal outcome
```

Native `on_tool_use` may defer realization until the first Sandbox-target call.
Concurrent first calls join one lifecycle lock and publish one durable binding.
ACP is eager because the opaque process itself needs the Environment. A2A-only
execution never realizes one; local mounts, repositories, memory or executable
Skills combined with outbound A2A fail before provisioning.

Failures are fail-closed: an insufficient Worker cannot claim; a lost claim is
retried under the existing lease/epoch fence; a failed Environment binding is not
published; a remote A2A transport failure is an attempt failure and never falls
back to local Native execution.

The Environment owns the Hand process and its live channel separately from the
durable Environment identity. If an idle container exec channel expires before a
tool request is written, that same owner stops the stale binding, starts one
replacement Hand in the existing Environment, and retries the undispatched call
once. A channel loss after dispatch remains indeterminate and is never replayed by
this lifecycle path; it continues through ADR-0044's existing recovery boundary.

The Session supervisor may proactively hibernate that same recreatable Hand after
a settled end-turn idle horizon. Hibernation releases only the process/channel;
the Environment, opaque durable binding, and workspace remain resident, and the
next tool call uses the same serialized replacement path. Awaiting human/tool
action is not classified as an end-turn idle. Terminal Session release remains a
different operation: it closes the owner and disposes the Environment, so it can
never lazily recreate a Hand. Full Pod/container suspension is disabled while the
workspace is backed by ephemeral storage; reclaiming it would violate continuity.

## Consequences

- Protocol availability is independent of Worker placement: external clients
  always enter through a resident Coordinator adapter.
- Native and ACP share one Environment and filesystem truth; outbound A2A owns
  neither, so it cannot silently accept local mounts or executable resources.
- Worker eligibility becomes stricter but explicit: every sandbox capability is
  checked before claim, while ranking remains replaceable after eligibility.
- Removing Agent/deployment Hand coordinates is a compatibility break for those
  internal configuration fields; no shadow migration path remains.
- Brain and Hand stay runtime roles rather than independently scheduled Worker
  kinds, keeping the service model smaller and avoiding cross-Worker Session
  consistency.

## Required invariants and tests

1. Every Session has at most one Environment and at most one Environment-derived
   Hand.
2. Every public normalized `method + path` has one adapter owner.
3. Every claim uses one `SandboxRequirements`/`SandboxCapabilities` predicate.
4. Agent publications and deployment config carry no Hand placement coordinate.
5. AllInOne has one registered Worker execution owner even without ACP.
6. A Session Environment may replace an expired Hand channel, but never owns more
   than one active binding and never replays a possibly dispatched tool call.
7. Idle hibernation is reversible and retains the Environment; terminal release is
   irreversible and never recreates a Hand. A provider may not suspend an
   Environment until its workspace has a durable continuation contract.

Cause/effect decision tables live beside the corresponding Rust/Python tests.
The architecture fitness suite rejects retired Hand selection symbols and
duplicate public route ownership.

## Reuse, modification and new code

- Reused unchanged: `ToolExecutor`, tool relay/channel framing, durable dispatch,
  Worker registry/ranking, Sandbox providers, Native/ACP/A2A attempt executors.
- Modified: Runtime Host Session context, Worker placement contract, AllInOne
  assembly, Coordinator ranking module, Agent/config registration shapes.
- New: neutral `SandboxRequirements` and execution/route ownership fitness checks.

No separate Brain Worker or Hand Worker service is introduced.
