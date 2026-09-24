# ADR-0083: Managed Cloud Session Durable Acceptance

- Status: Accepted
- Date: 2026-09-25
- Amends: ADR-0066, ADR-0072 and ADR-0075

## Context

A public Managed Cloud Session create must return after its complete durable
Session root and initial Event batch commit. Sandbox creation and Work execution
continue through the established lifecycle supervisor. The official [Managed
Agents Session lifecycle](https://platform.claude.com/docs/en/managed-agents/sessions)
starts provisioning after creation; a Session without initial events is idle,
while a nonempty initial batch starts it running.

In the local k3d environment, one node admitted 110 Pods. Ninety-eight
`awaken-h-*` Sandbox Pods were running, and Kubernetes emitted `Too many pods`
scheduling events. With 50 unarchived Shanzhao test Sessions, public Session
creation exceeded a 12-minute browser wait and timed out before returning an
ID. Archiving one verified idle test Session reduced the count to 49; the same
Cloud create then returned in 196 ms. Archiving another 24 verified test
Sessions reduced running Sandbox Pods from 98 to 81; the exact Shanzhao
create request then returned in 216 ms. This is capacity evidence, not a
claim that CPU tuning alone fixes first-use latency.

### Duplication and ownership review

| Responsibility | Existing authority | Decision |
|---|---|---|
| Atomic Session root, initial batch and idempotency | `ManagedSessionRepository` through `SessionApplication` | Reuse unchanged |
| Return durable root before physical effects | `SessionApplication::accept_session` | Use for local Cloud Managed create |
| Realize Sandbox and dispatch Work after root | Existing lifecycle supervisor | Reuse unchanged |
| Gate execution before physical readiness | Existing realization and Run admission | Reuse unchanged |
| Project public status | `ManagedState::wire_session_status` | Derive pending Cloud idle/running from durable active activity |
| New job, queue, pending-call or status store | None | Add none |

## Decision

The Worker-placement create remains on `AwaitRealization`: its conservative
pre-acknowledgement `rescheduling` projection and WorkQueue fence are already
qualified separately. Cloud completion uses the existing `Applied +
AcceptDurableRoot` branch of the closed create-effect kernel. An exact replay
returns repository truth without repeating any physical effect.

### Static and dynamic contract

The Managed HTTP adapter validates and lowers the public request. The Session
application alone commits the root. The supervisor alone realizes the frozen
Sandbox and resumes the same Session; the Runtime Host still gates Sandbox
tools and model work on readiness. No application database becomes a second
Session execution authority.

```text
POST /v1/sessions -> validate complete Agent/Environment/Resource inputs
                  -> atomic root + initial Event batch + idempotency receipt
                  -> return Session ID and logical idle/running status
                  -> existing supervisor wakes, realizes and activates
                  -> admitted Work runs or existing recovery records a failure
```

For a local Cloud root in `Preparing` or `Activating`, the same persisted
`active_activity_epochs` set determines the compatible logical state: empty
means `idle`, nonempty means `running`. A durable `Rescheduling` state remains
`rescheduling`; `ActivationFailed` is terminal. Registered Worker placement
keeps its earlier status mapping. These wire states do not grant an executor
permission to enter the Sandbox before realization.

Pre-root validation or repository failure returns its existing typed error
without a partial Session. After an Applied root, physical failure is recovered
through the existing supervisor and its exact fences. A client retry with the
same idempotency key addresses that root; it never authors a second Session.

## Consequences

### Verification and limits

The new Managed test decision table covers no initial event versus one
message, blocked physical projection, durable Preparing root, idle/running
response, and exact replay with no duplicate Runtime effect. Existing
application tests cover cold accepted-root recovery and the Worker-placement
test retains its pre-acknowledgement fence. HTTP adapter tests cover the
official initial Event and response shape.

Durable acceptance removes Sandbox readiness from the create request's
critical path. It cannot manufacture Kubernetes capacity: if the node has no
Pod slot, the accepted Session still waits for its Sandbox before work can
advance. The local test harness archives completed test Sessions; production
deployments must provide sufficient Pod capacity or use the existing frozen
idle-retention policy. This ADR adds no second eviction mechanism.
