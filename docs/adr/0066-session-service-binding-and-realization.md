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

## 2026-08-13 amendment: hosted application MCP credential admission

A trusted hosted application that contributes an authenticated MCP endpoint
must author that bearer through the existing Credential/Vault aggregate before
it creates the Managed Session. The identity is the exact
`(Workspace, application authority, normalized MCP target)` tuple. Control
derives stable opaque Vault and credential-source ids; callers cannot supply a
second identity or persist a Flow-local credential mirror.

The command reuses the Managed `Idempotency-Key` contract. Its payload is
write-only and the durable credential row remains secret-free. A replay of the
same key and payload returns the same Vault id, source id, and revision. A new
key may rotate only the material revision in place; it cannot change the tuple
or create a parallel desired-state source. The existing credential mutation
intent and source CAS select one concurrent winner. Conflicting tuple state,
idempotency-key reuse with another payload, and stale concurrent rotation fail
with `409`.

Static ownership remains:

```text
hosted application -> Managed credential command (wire only)
                   -> VaultState (normalization/projection)
                   -> CredentialRepo + SecretStore (identity, CAS, custody)
                   -> SessionApplication (exact source/revision pin only)
```

Dynamic order remains one-way:

```text
POST /v1/config/application-mcp-credentials
                        -> receive stable vault/source/revision
                        -> POST /v1/sessions with that vault id
                        -> normalize MCP target once
                        -> pin the exact source revision
                        -> realize the existing MCP generation protocol
```

Failure before Session creation creates no Session. A lost credential-command
response replays the same key. A lost Session response replays the Session's own
independent idempotency key. Neither failure falls back to an embedded/local
credential or a second Session implementation.

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
