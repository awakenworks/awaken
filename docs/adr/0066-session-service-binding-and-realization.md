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

## 2026-08-13 amendment: application MCP credential admission

A trusted application that contributes an authenticated MCP endpoint
must author that bearer through the existing Credential/Vault aggregate before
it creates the Managed Session. The identity is the exact
`(Workspace, application authority, normalized MCP target)` tuple. Control
derives stable opaque Vault and credential-source ids; callers cannot supply a
second identity or persist an application-local credential mirror.

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
application -> Managed credential command (wire only)
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

## 2026-08-16 amendment: authenticated MCP credential delivery

Authenticated MCP freezes one delivery mode into each exact attachment
generation. The two supported intents are:

```text
ClientInjection   -> an explicitly authorized MCP client process receives the
                     credential through its declared private delivery field
GatewayMediation  -> an installed egress mediator receives the credential
                     reference and the workload sees only a mediated route
```

The mode is selected before materialization from the exact plaintext holder and
installed realization mechanism. It is never inferred from route availability
and never changes as a runtime fallback. `ClientInjection` accepts only an
authorized Workload holder combined with `ProcessProtocolField`: material is
placed in the process-private ACP `session/new` field and never represented as
an OS environment variable, private file, durable ACP configuration, or URL.
`GatewayMediation` accepts only a Platform holder combined with `PlatformRelay`.
Historical environment/file/relay/provider wire values remain readable but are
not reclassified into either mode.

The Session aggregate remains secret-free. Stage receipts must match the exact
generation binding, selected holder, realization mechanism and delivery mode
before activation. Deployment-specific egress mediation is supplied through
the existing realization port and cannot become another desired-state owner.

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

## 2026-08-24 amendment: hosted Session identity and credential adoption

An application that must issue a Session-bound credential before Session create
uses the Managed adapter's exported
`managed_session_id_from_idempotency(owner_scope, key)` function. The public
create path calls that same pure function; there is no copied formula or
metadata lookup identity. This predicts only the opaque address. The scoped
Session repository remains the payload-match and replay authority, so the
application still performs direct `GET /v1/sessions/{id}` and, only on `404`,
the ordinary idempotent `POST /v1/sessions`.

The application MCP credential receipt now carries the existing closed
credential-adoption state, `converged | pending`. Creation has no predecessor
generation and is converged. Rotation synchronously attempts the exact durable
outbox event through the already configured `ManagedCredentialRolloutTarget`.
Only target convergence followed by the repository's exact-event
acknowledgement returns `converged`; an absent target, a busy Session, or target
failure returns `pending` and leaves the same event for the existing supervised
reconciler. Command responses read only the existing primary event id; full
outbox enumeration remains exclusive to that reconciler and is never amplified
per HTTP replay. A hosted application must wait/retry on `pending` before
starting a Run that requires the new bearer.

The write command also carries a required positive, caller-monotonic
`credential_generation`. The existing deterministic material reference records
that generation beside the command-key fingerprint and before its existing
writer-attempt fence; no receipt row or counter is added. A legacy reference is
generation zero and may be upgraded by the first positive generation. Only the
exact current `(generation, key fingerprint, sealed material)` replays. A
strictly newer generation uses the existing WAL/CAS rotation; an equal
conflicting or delayed older generation fails before writing. This narrows the
earlier “new key rotates” statement: a key change alone is never authority to
restore an older bearer.

This adds no Session mapping table, metadata identity, credential store, route,
scheduler, lease, compatibility fallback, or second rollout vocabulary. The
dynamic order is:

```text
derive canonical Session id -> direct GET
  404 -> issue Session-bound MCP bearer with positive monotonic generation
       -> enter/replay/rotate existing Vault source
       -> receipt pending: retry after existing Idle-only adoption
       -> receipt converged: idempotent Session create or replay
  200 -> reuse the exact durable Session
```
