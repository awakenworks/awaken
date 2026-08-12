# ADR-0066: Session Service Binding and Realization

- Status: Accepted
- Date: 2026-08-02
- Current Worker-placement decision: [ADR-0075](0075-unified-managed-session-worker-execution.md)

## Context

Session desired state and physical effects require one durable owner, one root
CAS, and one phase protocol across local and remote topology. ADR-0075 later
removed the late Worker-input portion without changing those authorities.

## Decision

A Session has one aggregate root revision, one immutable finalized baseline,
one versioned Resource state, and one versioned MCP attachment set. Desired
state is normalized under `SessionApplication` and committed through the one
`ManagedSessionRepository` root CAS.

MCP desired state uses full replacement. Logical name precedence is Session
over Agent; selected targets must be unique. Physical realization is a phased,
lease-fenced protocol:

```text
Requested -> Realizing -> Active -> Draining -> Removed
                  \-> Failed
```

`SessionRealizationLease` owns the physical projection. Per-generation claims
and receipts bind the exact Session, attachment, generation, target, holder,
Worker incarnation, epoch, and expiry. The shared `drive_session_realization`
driver performs stage, activate, publish, acknowledge, drain, and failure for
both local and remote adapters.

## Durable structure

```text
PersistedSession
├── revision
├── baseline: Preparing | Frozen
├── resources: SessionResourceState
├── mcp: SessionMcpAttachmentSet
├── realization: Option<SessionRealizationLease>
├── execution
└── disposition
```

The scoped persistence envelope carries Workspace ownership beside the
aggregate. Protocol DTOs derive from durable state and do not become another
authority.

## 2026-08-12 amendment

Session creation no longer waits for Worker-authored input. The complete
creation command contains Environment, model/runtime identity, mounts,
environment values, prompts, Resources, network policy, and initial MCP
candidates. Finalization consumes that command before WorkQueue projection.

The removed late-input path is not a compatibility mode. Existing realization
leases, MCP generations, root CAS, and recovery remain unchanged. Worker
placement and the relationship between Work leases and Run claims are now owned
by ADR-0075.

## Static boundaries

| Boundary | Owns | Rejects |
|---|---|---|
| Session application | normalization, authorization edge, root commands | direct repository writes from adapters |
| Session repository | CAS, idempotency, outbox, recovery scan | protocol status authority |
| MCP attachment set | desired generations and transitions | plaintext material |
| realization driver | ordered physical phases | desired-state authoring |
| protocol adapter | wire mapping | independent merge/update semantics |

## Failure and retry

- normalization conflict fails before root insertion;
- a stale root revision or realization lease commits nothing;
- stage-before-CAS failure records the existing retry state;
- stale physical receipts are disposed or rejected and cannot publish;
- response loss replays the same command/generation identity;
- terminal Session cleanup fences admission and drains/removes physical state.

## Consequences

Resource, MCP, environment, and execution state remain separate value/state
objects under one Session root. Local and remote realization share one phase
protocol, and no adapter carries a second desired-state registry.
