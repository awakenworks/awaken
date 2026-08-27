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

## 2026-08-27 amendment: immutable post-create mutation authority

### Decision and static structure

`SessionBaseline.mutation_policy` is the one durable authority for which
Agent/Resource authoring operations may change a Session after creation. The
closed `SessionMutationPolicy` value is selected in the complete
`ControlSessionCreationInputs`, fingerprinted into every non-Managed baseline,
and frozen by `SessionCreationIntent::finalize` before the first root insert.
No protocol, product adapter, metadata field, or current caller identity may
reinterpret it later.

| Policy | Creation owner | Post-create authoring admitted |
|---|---|---|
| `Managed` | ordinary Managed Session creation and historical rows | the existing public Session Agent update, complete Resource-manifest mutation, and Repository credential update paths |
| `Frozen` | profiled WorkUnit and built-in fixed-purpose Sessions | none of those Agent, Resource, or Repository-credential mutations |
| `FileResources` | profiled interactive Sessions | only File binding attach, replace, or delete through the ordinary item-level Resource API; every non-File input and exact Skill pin remains equal |

Title, metadata, budget, Events/Runs, archive, and delete remain owned by their
existing Session commands and state machines. They do not rewrite the baseline
and are not reclassified as Agent/Resource authoring by this policy.

```text
ordinary Managed create ------------------------------> Managed
private profiled mode: WorkUnit ----------------------> Frozen
private profiled mode: Interactive -------------------> FileResources
                                                            |
complete create command -> SessionCreationIntent::finalize  |
                         -> immutable SessionBaseline <-----+
                                      |
public Agent/Resource command --------+-> policy gate -> existing root CAS
Vault lifecycle event -> SessionMcpUpdate::CredentialLifecycle
                                      `-> bounded existing MCP generation path
```

The existing `SessionUpdateCommand`, Resource manifest command, and Repository
credential ingress remain the command owners. `SessionMutationPolicy` adds no
repository, route, lifecycle, generation, or realization mechanism.

`SessionMcpAttachmentSet::desired_attachments()` is the sole selector for the
MCP desired set used by creation, typed updates, comparison, and realization.
Public replacement and credential lifecycle commands do not maintain another
filtered view or choose current MCP truth independently.

### Credential lifecycle is not public re-authoring

The Managed adapter lowers a public MCP desired-set change to
`SessionMcpUpdate::PublicReplacement`. Only `Managed` admits that operation.
The existing Vault rollout instead uses the distinct typed
`SessionMcpUpdate::CredentialLifecycle` command, so an immutable profiled
Session cannot prevent an authorized credential rotation or revocation.

That lifecycle command is deliberately narrower than public replacement. It may
replay or monotonically advance the same source revision, or remove only the
attachments affected by revocation. It cannot add an attachment, change name,
target, prompt exposure, origin, or an unrelated credential, and it cannot be
combined with title, metadata, budget, or tool mutation. The Vault child's
`ManagedCredentialLifecycle` and its WAL/CAS remain the credential authority;
the Session command only adopts the resulting exact MCP generation.

Repository credential replacement is product authoring rather than rollout
adoption. It is admitted only by `Managed`, and a profiled Session rejects it
before Vault ingress or another external effect.

### Dynamic behavior and failure boundary

```text
create
  -> receive the complete Agent profile, direct Resource attachments,
     Repository inputs, MCP candidates, Environment, tools, and policy
  -> resolve and normalize once
  -> finalize baseline + initial Resource/MCP truth
  -> atomically insert one root and its IdempotencyRecord

post-create Agent/Resource request
  -> load the frozen baseline and current Resource/MCP truth
  -> classify the typed mutation against SessionMutationPolicy
  -> denied: return the existing typed bad-request owner; no Vault, catalog,
     realization, Runtime, or root-CAS effect
  -> admitted: reuse the existing command, root CAS, realization, and receipt

credential rollout
  -> receive one exact Vault lifecycle event
  -> validate same-source monotonic rotation or exact affected revocation
  -> reuse the existing MCP replacement/generation/realization path
  -> reject topology or unrelated-credential widening before root mutation
```

The private whole-manifest extension cannot be used to complete or mutate a
profiled Session after creation. `FileResources` changes enter through the
ordinary item-level File verbs, whose lowering produces a complete candidate
manifest; the Session application then proves that only File bindings changed.

Policy failures reuse the existing `SessionUpdateError::Rejected`,
`SessionResourceManifestError::Rejected`, or
`SessionPreparationError::Rejected` owner and its neutral `RunError`. No new
wire error union or adapter-owned authorization state is introduced.

### Repository receipt identity remains atomic

The existing `ManagedSessionRepository` remains the only classifier for a
Session id, owner scope, tombstone, aggregate, and operation receipt. Its create
transaction writes one revision-1 receipt with the new root and returns
`SessionCreateResult::{Applied, Replayed}`; the application never decides replay
from metadata or from separate owner/receipt/aggregate reads. The same atomic
snapshot is exposed for deterministic preflight through `replay_create`.

A create receipt must name committed revision 1 even when an exact replay later
returns a more advanced current aggregate. A mutation receipt must name revision
2 or later and exactly equal that command's expected next revision. Reusing a
receipt across an operation or expected revision is
`IdempotencyMismatch`. A receipt ahead of durable truth, a dangling receipt, or
simultaneous live and tombstone identity is `Corrupt`; the application maps that
to its typed internal failure and performs no mutation or external effect.

### Compatibility and maintenance cutover

An absent policy decodes as `Managed`. `Managed` remains omitted when serialized
and retains the historical baseline fingerprint algorithm and ordinary Managed
wire/update request fingerprints. Non-Managed fingerprints are domain-separated
by the exact policy. Historical profiled rows are therefore not guessed or
silently tightened; they remain `Managed`, and products that need a stricter
guarantee create a new post-cutover Session.

The baseline independently freezes the Managed request's exact
`SessionSystemPromptSelection::{Inherit, Clear, Replace}`. `Inherit` is the
omitted legacy default and retains the historical fingerprint; `Clear` and
`Replace` are explicit, domain-separated authoring facts. A later GET resolves
that frozen choice against the pinned Agent publication, so an explicit clear
cannot be mistaken for inheritance. This is baseline projection through the
existing realization contract, not another mutation policy or Runtime Host
authority.

The first non-Managed row and atomic profiled receipt are a new beta aggregate
contract. A default lets the new reader consume old Managed rows, but old
binaries cannot safely read and rewrite the new field. Reuse the existing
ADR-0075 beta maintenance cutover: close Flow/public admission, drain and stop
every old Coordinator writer, deploy the new readers/writers, complete the
canonical lifecycle-supervisor validation, and only then create profiled
Sessions and reopen admission.

This cutover is full-stop and forward-only. It does not adopt or backfill an old
profiled metadata fingerprint or its default aggregate-payload create receipt.
After upgrade, retrying such an old deterministic profiled identity returns the
typed HTTP 409 conflict; Flow creates a new post-cutover identity. Ordinary
public Managed metadata-backed replay remains unchanged. There is no
rolling-write compatibility mode, guessed receipt equivalence, or Cloud-side
migration owner.

### Cause-effect decision table

| Rule | Frozen policy | Mutation cause | Effect |
|---|---|---|---|
| P1 | absent / `Managed` | ordinary public mutation | preserve legacy fingerprint/wire and reuse the existing command |
| P2 | `Frozen` or `FileResources` | public tools or MCP replacement | reject before projection or root mutation |
| P3 | `Managed` | valid complete Resource replacement | reuse the canonical Resource CAS and realization |
| P4 | `Frozen` | any new Resource replacement | reject before Resource/Vault/realization effects |
| P5 | `FileResources` | only File bindings differ | admit the ordinary item-level operation and canonical manifest CAS |
| P6 | `FileResources` | non-File input or exact Skill pins differ | reject without durable or physical change |
| P7 | non-Managed | Repository credential replacement | reject before Vault ingress or Repository effects |
| P8 | any | valid typed credential rotation/revocation | adopt only the exact affected MCP generation |
| P9 | any | lifecycle command widens topology or an unrelated credential | reject before root mutation |
| P10 | new profiled create | complete inputs valid | freeze policy and all desired inputs in one original root insert |
| P11 | Managed create | system prompt is omitted | freeze `Inherit`, omit the field, and retain the historical fingerprint |
| P12 | Managed create | system prompt is explicitly cleared | freeze `Clear`, return no inherited prompt, and use a distinct fingerprint |
| P13 | Managed create | system prompt is replaced | freeze `Replace(value)`, return that exact value, and use a distinct fingerprint |
| P14 | post-cutover profiled retry | only old metadata/default-payload identity exists | return typed 409; do not adopt, backfill, or perform effects |
| P15 | any receipt replay | receipt is dangling, ahead, or paired with two identities | return typed internal corruption; do not mutate or perform effects |
