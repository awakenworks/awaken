# ADR-0066: Session Baseline and Dynamic MCP Attachments

- Status: Accepted
- Date: 2026-07-24
- Accepted: 2026-07-25
- Builds on: [ADR-0041](0041-sandbox-execution-environment-provider.md)
  (one Sandbox/process provisioning seam),
  [ADR-0056](0056-sandbox-reuse-two-orthogonal-volumes.md)
  (Session-owned live environment), and
  [ADR-0065](0065-recoverable-embeddable-remote-worker.md)
  (one frozen claim-time application input and public Worker assembly)
- Clarifies ADR-0041: dynamic MCP routing and stream lifecycle remain outside
  `awaken-provisioning-contract`
- Amends ADR-0065: claim-time application MCP declarations are submitted as one
  claim-fenced, secret-free contribution before Session activation and become
  generation 1 of their logical attachments; later MCP changes are independent
  Session commands and never mutate or resubmit `ApplicationSessionPlan`
- Credential policy:
  [ADR-0067](0067-credential-custody-model-exposure-and-secret-delivery.md)
- Target guardrail: G42

## Context

Awaken already has most mechanisms needed by dynamic MCP, but they currently form
overlapping authorities:

- `SessionInit.mcp_servers` persists one MCP-specific initialization pin;
- `ApplicationSessionPlan.mcp_servers` supplies a second claim-time MCP input;
- `PreparedMcpServer` holds an already-materialized bearer in Runtime Host;
- Managed Session creation chooses either a published Agent credential binding or
  scans Session `vault_ids` by normalized URL;
- `McpManager` owns a frozen list of connected servers, while Runtime already has
  safe-boundary capability refresh;
- `SessionRuntimeSlot` keeps process-local MCP state beside the durable Managed
  Session repository;
- Managed networking, Sandbox networking, `deny_egress`, and `ThreadEgress`
  overlap before reaching the existing `NetworkPolicy`.

The previous form of this ADR attempted to solve those paths with a generic
`ServiceDefinition`/`ServiceSnapshot`, a `SessionProvisioningSpec` that contained
services, and a public `SessionServiceRealizer`. That design was internally
inconsistent: the specification was described as immutable and compiled once,
while the services inside it were hot-addable. It also projected Model and
Repository access into the same Service aggregate, creating a second authority
beside `ResolvedModelCandidate` and the Resource aggregates.

The repository's existing `SessionResourceState` already demonstrates the useful
lifecycle: persist desired intent, realize externally, then commit or roll back.
It also proves that Resource attachments are dynamic and therefore do not belong
inside an immutable Session baseline.

## Decision

### D1: one Session aggregate separates immutable and versioned facts

The Session aggregate is:

```rust
struct PersistedSession {
    session_id: SessionId,
    revision: SessionRevision,
    baseline: SessionBaselineState,
    resources: SessionResourceState,
    mcp: SessionMcpAttachmentSet,
    realization: Option<SessionRealizationLease>,
    execution: SessionExecutionState,
    disposition: SessionDisposition,
}

enum SessionBaselineState {
    Preparing(SessionCreationIntent),
    Frozen(SessionBaseline),
}

struct SessionCreationIntent {
    control_inputs: ControlSessionCreationInputs,
    application: ApplicationContributionState,
}

enum ApplicationContributionState {
    Required,
    Absent,
    Committed {
        fingerprint: ApplicationPlanFingerprint,
        input: ApplicationSessionInput,
    },
}

struct SessionBaseline {
    fingerprint: SessionBaselineFingerprint,
    environment: EnvironmentSnapshot,
    mcp_authoring: SessionMcpAuthoringContext,
    agent_id: AgentId,
    model: ModelId,
    runtime: RuntimeSelection,
    application: Option<ApplicationContributionReceipt>,
    delegate_ids: Vec<AgentId>,
    toolsets: Vec<ToolsetPolicy>,
    mounts: Vec<MountRequirement>,
    env: Vec<EnvVar>,
    prompts: Vec<String>,
}

struct SessionMcpAuthoringContext {
    ordered_vault_ids: Vec<VaultId>,
}

struct EnvironmentSnapshot {
    environment_id: EnvironmentId,
    revision: EnvironmentRevision,
    config_fingerprint: EnvironmentFingerprint,
    sandbox: SandboxRequirements,
    packages: EnvironmentPackages,
    network: NetworkPolicy,
    credential_realization: CredentialRealizationProfile,
}
```

`SessionExecutionState` owns preparation, activation, activity, failure, and
termination. `SessionDisposition` independently owns active, archived, deleting,
and deleted visibility. The Session application is the only command owner for
both axes; stores perform compatibility decoding and root-CAS persistence, while
protocols derive `status` and `archived_at`. Thread/Run `RunState`, Host slots,
and physical cleanup state remain separate bounded-context facts and must not be
folded into either Session enum.

The opaque ownership scope remains beside the serialized aggregate in the
repository row/`ScopedPersistedSession` envelope defined by ADR-0051. It is not
a `PersistedSession` field and is never read by Session domain behavior.

`SessionMcpAuthoringContext` freezes only the ordered, secret-free compatibility
references needed when the Managed update API later supplies an MCP definition
without an explicit published credential. It contains no MCP desired state,
credential material, authorization decision, or mutable Vault snapshot. A new
generation performs a new exact resolution through the sole normalizer; an
existing generation never re-resolves its credential.

The Environment registry adds a monotonic revision to its existing mutable
record. One Managed compiler canonicalizes the exact revision into
`EnvironmentSnapshot`, including the effective safe-intersection
`NetworkPolicy`, Sandbox requirements, and credential realization profile. The
snapshot fingerprint covers those normalized fields. Later Environment edits
affect only new Sessions; Runtime Host and providers never re-read the latest
Environment record for an existing Session.

The credential realization profile freezes separate exact holders for inference,
MCP, and Resource execution. This does not make Resource state part of the
immutable baseline: the baseline supplies the holder decision, while each
Repository generation persists its own exact credential access/holder pin in
`SessionResourceState`. A hot Resource replacement changes only that generation
and the root Session revision; it never mutates or recompiles the baseline.

The initial insert persists a temporary `Preparing(SessionCreationIntent)` so a
claim-time registered application can contribute before any external
realization. That intent is not an alternative runtime specification: only the
creation compiler reads it, one root mutation consumes it, and the same mutation
replaces it with `Frozen(SessionBaseline)` plus generation 1 Resource/MCP state.
Raw authoring inputs are then gone. A Session whose composition has no registered
application records `Absent` explicitly and can finalize immediately.

A required contribution has no independent wall-clock timeout or background
expiry authority. While it is absent, the Session remains inert in `Preparing`:
no Runtime preparation, Resource/MCP realization, or idle lifecycle fact occurs.
The ordinary idempotent Session delete command is the sole cancellation path; it
commits the terminal delete/tombstone transition, after which a late contribution
returns not found and cannot resurrect the aggregate. A future automatic
retention policy, if required by operations, must invoke that same command rather
than add another preparation state machine.

`SessionBaseline` is immutable after this finalization CAS and before any
external realization. It contains only facts whose lifecycle is frozen for that
Session. `SessionResourceState` remains the existing versioned authority for live Resource add/update/delete.
`SessionMcpAttachmentSet` becomes the one versioned authority for initial and
later MCP attachments.

Skill pins are not copied into `SessionBaseline`. ADR-0063's
`ResolvedSessionResources.skills` remains their only durable authority and is
carried by `SessionResourceState`; the baseline fingerprint therefore cannot
become a second Skill-version truth. Application-contributed mounts, environment
values, and prompts remain baseline facts because they are not Resource
attachments and have no independent dynamic lifecycle in this slice.

The compiler may return one transient creation value:

```rust
struct CompiledSessionCreation {
    baseline: SessionBaseline,
    initial_resources: ResolvedSessionResources,
    initial_mcp: Vec<McpAttachmentDraft>,
}
```

This value coordinates the finalization transaction; it is not persisted as a
second complete Session specification. Initial Resource and MCP values become
generation 1 in their respective versioned states in the same CAS that freezes
the baseline. They are not copied into the baseline.

### D2: the first bounded scope is MCP, not a generic Service domain

The first target model is intentionally MCP-specific:

```rust
struct SessionMcpAttachmentSet {
    revision: McpSetRevision,
    desired_fingerprint: McpDesiredSetFingerprint,
    desired_names: Option<Set<McpName>>,
    attachments: Vec<SessionMcpAttachment>,
}

struct SessionMcpAttachment {
    attachment_id: McpAttachmentId,
    name: McpName,
    generation: McpGeneration,
    target: McpTarget,
    credential: Option<CredentialAccess>,
    selected_plaintext_holder: Option<PlaintextHolder>,
    state: McpAttachmentState,
    publication_acknowledged: bool,
    realization: Option<McpRealizationClaim>,
    attempts: u32,
    last_error: Option<String>,
}

struct McpRealizationClaim {
    realization_id: McpRealizationId,
    runtime_incarnation: SessionRuntimeIncarnation,
    lease_epoch: u64,
    lease_expires_at: Timestamp,
    stage_idempotency_key: IdempotencyKey,
}

enum McpAttachmentState {
    Requested,
    Realizing,
    Active,
    Draining,
    Removed,
    Failed,
}
```

The identity and visibility invariants are:

- `(attachment_id, generation)` is unique for the lifetime of a Session;
- generations for one `attachment_id` increase monotonically and never wrap;
- at most one generation of an attachment is `Active` and visible;
- at most one visible generation owns a Session-local MCP name;
- replacement may temporarily retain the old `Active` generation while the new
  generation is `Requested` or `Realizing`;
- `Failed` and `Removed` are terminal;
- wire `mcp_servers` is derived from visible attachment generations and is not a
  separately persisted authority.
- `publication_acknowledged` is true only after Runtime acknowledges the exact
  active claim; an `Active` generation with a false acknowledgement is durable
  recovery work, not evidence that the process-local projection exists.

Names and generations fence tool discovery and calls. A call is valid only for
the exact active attachment id and generation that produced it. Exhausted root,
set, or generation counters fail closed rather than wrap.

Model access remains authoritative in `ResolvedModelCandidate`. Repository,
File, Memory, and Skill remain Resource-domain concepts. They may reuse neutral
credential, network, receipt, ownership, lease, and failure-close mechanisms,
but they do not become MCP attachments or a common Service aggregate.

Generic HTTP, SSE, WebSocket, LLM HTTP, Git, and a general Service context are
deferred until a second dynamic protocol proves the same aggregate lifecycle.

### D3: all MCP authoring inputs normalize once

Published and compatibility inputs converge at the Managed anti-corruption
boundary:

```text
published Agent MCP binding ───────┐
inline MCP + Session vault_ids ────┼── McpAttachmentNormalizer
application MCP input ─────────────┘
                                              │
                                              ▼
                                  exact McpAttachmentDraft
```

`ApplicationSessionPlan` replaces `mcp_servers: Vec<PreparedMcpServer>` with
`mcp_inputs: Vec<McpAttachmentInput>`. That input is secret-free and contains no
bearer, relay handle, connection, or realized transport. The frozen application
plan remains fingerprint-idempotent. Because the plan is produced only after a
remote Worker owns a Run claim, it does not participate in the earlier Session
insert transaction. Instead the Worker submits one `ApplicationSessionContribution`
to the Coordinator-owned Session application service while the Session is still
`Preparing`:

```rust
struct WorkerApplicationContributionCommand {
    run_claim: RunClaimRef,
    worker_identity: WorkerIdentity,
    contribution: ApplicationSessionContribution,
}

struct ApplicationSessionContribution {
    session_id: SessionId,
    application_fingerprint: ApplicationPlanFingerprint,
    input: ApplicationSessionInput,
}

struct ApplicationSessionInput {
    mounts: Vec<MountRequirement>,
    env: Vec<EnvVar>,
    prompts: Vec<String>,
    mcp_inputs: Vec<McpAttachmentInput>,
    network_restriction: Option<NetworkPolicy>,
}
```

The Control transport edge verifies the envelope's exact claim/epoch, Worker
identity, Session/Run correlation, and application fingerprint, then strips the
envelope before invoking the Session application port. The domain command has no
claim or Worker vocabulary. It persists only `ApplicationSessionInput` and its
fingerprint into the preparation intent, and runs the single
creation compiler over Control and application inputs. Re-delivery of the same
contribution is a replay; a different payload under the same fingerprint fails
closed. One root CAS freezes the baseline, safely intersects the application
network restriction, and creates generation 1 Resource/MCP state. The Session
cannot begin external realization until this finalization commits. No
Worker-local baseline, overlay, or desired-state registry is allowed.

`RunClaimRef` is boundary command data and never enters `PersistedSession`. The
Session aggregate contains no dispatch owner, epoch, transport, or Worker
authorization vocabulary; those are validated before invoking its command.

Normalized drafts retain only an origin tag needed for deterministic precedence
and audit:

```rust
enum McpAttachmentOrigin {
    Session,
    Application,
    Agent,
}
```

They do not retain raw authoring DTOs. The preparation intent is consumed after
the one complete precedence calculation, so recovery reads the frozen baseline
and exact drafts rather than rerunning authoring logic.

The normalizer applies one deterministic rule set:

1. canonicalize HTTP(S) targets by lower-casing scheme/host, removing default
   ports and trailing slashes, and preserving path, query, non-default port, and
   subdomain;
2. reject userinfo, fragments, unsupported schemes, empty names, duplicate names,
   duplicate canonical targets, and conflicting definitions rather than merge
   them silently;
3. precedence is explicit Session inline input, then application input, then
   published Agent default; a higher-precedence entry replaces a lower one only
   by exact name, while a same-target/different-name collision is rejected;
4. an explicit published credential binding wins for that definition; otherwise
   compatibility `vault_ids` are scanned in caller order and credential id breaks
   ties inside one Vault;
5. the result freezes exact credential id, revision, `CredentialUsage`, execution
   policy, target fingerprint, and input payload fingerprint; archived, revoked,
   ambiguous, or revisionless protected credentials are rejected;
6. no matching credential means explicitly unauthenticated MCP access, never a
   later Host lookup.

The sole public hot-update command is the existing Managed Agents full
replacement shape, `POST /v1/sessions/{id}` with `agent.mcp_servers`. The Managed
application service normalizes the requested target set and diffs it against the
current attachment set into internal add, replace, remove, or no-op aggregate
commands. It never mutates the wire projection directly. The response and
`session.updated` event derive `agent.mcp_servers` from durably visible active
generations. No second public MCP CRUD surface is introduced in this slice.

The edge maps the optional standard `Idempotency-Key` header to the same
repository idempotency table used by root mutations, returns the stable operation
id in `X-Awaken-Operation-Id`, and returns the committed root revision as `ETag`.
A compatibility client that
supplies no key does not gain transport-level response-loss deduplication;
instead the full-replacement command is convergent. Its canonical desired-set
fingerprint is persisted in `SessionMcpAttachmentSet`, and replaying the same
target adopts the matching nonterminal operation or returns no-op once the same
set is active, without allocating duplicate generations. Public clients need not
understand the internal root revision. The application service performs bounded
CAS retry by reapplying the same full-replacement command to the latest
aggregate; an explicit `If-Match` extension, when present, disables retry and
maps a mismatch to conflict.

The final successful command mutation stores the request hash under the derived
operation key. A replay with the same key and hash returns without another
generation, Runtime effect, root revision, or `session.updated` event; reuse with
another hash is a conflict. Failed commands do not forge a success receipt. A
retry reuses or replaces durable nonterminal work, while a terminal `Failed`
generation is never resurrected and causes allocation of generation N+1.

URL matching and Vault order remain only Managed compatibility rules. No Host,
relay, Native, ACP, or recovery path may repeat credential selection or look up a
newer revision.

After all callers migrate, `SessionInit.mcp_servers`,
`ApplicationSessionPlan.mcp_servers`, and `PreparedMcpServer` cease to be
authorities and are deleted. The current wire-only
`ManagedState::update_session(..., mcp_servers)` assignment is also deleted; it
must not report a successful MCP change without changing durable attachment and
Runtime state. No long-lived dual-write path is accepted.

### D4: one root Session revision is the consistency boundary

`ManagedSessionRepository` remains the sole Session repository. All Session
writes use one root-revision mutation contract rather than a separate MCP
registry, MCP repository, or direct field update:

```rust
struct SessionMutation {
    expected_revision: SessionRevision,
    idempotency_key: IdempotencyKey,
    payload_hash: PayloadHash,
    payload: SessionMutationPayload,
    lifecycle_facts: Vec<SessionLifecycleFact>,
}

enum SessionMutationPayload {
    Replace(PersistedSession),
    Delete(SessionTombstone),
}

struct SessionTombstone {
    session_id: SessionId,
    deleted_revision: SessionRevision,
    deleted_at: Timestamp,
}

struct IdempotencyRecord {
    key: IdempotencyKey,
    payload_hash: PayloadHash,
}

enum SessionMutationResult {
    Applied { new_revision: SessionRevision },
    Replayed { new_revision: SessionRevision },
    Conflict { current_revision: SessionRevision },
    IdempotencyMismatch,
}

trait ManagedSessionRepository {
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<SessionLifecycleFact>,
    ) -> Result<SessionRevision, SessionRepositoryError>;

    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError>;
}
```

Resource mutation, MCP mutation, and Session environment binding converge on the
same root revision. Resource- and MCP-local revisions still fence their own
generations, but they do not replace aggregate-level optimistic concurrency.
The current process-local Resource mutation mutex cannot be a cross-process
consistency authority and is removed after repository CAS is adopted.

Creation is insert-only. `commit_mutation` atomically writes the Session row,
root revision, Resource/MCP state, idempotency record, and lifecycle/outbox
facts. Repeating the same key and payload returns the committed result; reusing a
key with another payload fails closed. Environment binding, lifecycle change,
archive/delete, Resource prepare/commit/rollback, MCP mutation, and recovery
reconciliation may use specialized command helpers, but each helper must compile
to this same expected-revision transaction and may not issue a direct Session-row
update.

Delete atomically replaces the live aggregate with a durable tombstone and
commits its terminal fact and idempotency receipt. The public read model treats
the tombstone as not found. Physical purge is a later idempotent retention action
that cannot remove the idempotency/outbox evidence before their retention
windows. This makes response-loss retry deterministic instead of asking a
deleted row to carry its own receipt.

During migration, legacy repository methods may exist only as thin wrappers that
compile to `create` or `commit_mutation`; SQLite/Postgres and in-memory stores
must have one underlying transaction implementation. Callers then migrate and
the wrappers, narrow direct updates, and process-local Resource mutation mutex
are deleted. A second storage algorithm hidden behind compatibility methods is
not permitted.

### D5: one public MCP realization port, with relay kept private

The first feature slice after contract closure extends the existing Session
application/Host seam narrowly with an explicit stage/commit/publish protocol:

```rust
struct SessionRealizationLease {
    session_id: SessionId,
    owner: SessionRuntimeOwnerRef,
    incarnation: SessionRuntimeIncarnation,
    epoch: u64,
    expires_at: Timestamp,
}

trait McpAttachmentRealizer {
    async fn stage_mcp_attachment(
        &self,
        request: StageMcpAttachment,
    ) -> Result<McpRealizationReceipt, RunError>;

    async fn publish_mcp_generation(
        &self,
        generation: McpGenerationRef,
    ) -> Result<(), RunError>;

    async fn drain_mcp_generation(
        &self,
        generation: McpGenerationRef,
    ) -> Result<(), RunError>;
}

enum McpRuntimeError {
    UnsupportedTransport,
    UnsupportedHolder,
    OutOfPolicyTarget,
    StaleOwnership,
    UnknownGeneration,
    GenerationNotActive,
    StageFailed,
    DrainTimedOut,
}
```

`SessionRuntimeOwnerRef` and `SessionRuntimeIncarnation` are opaque Session
application values. The local adapter maps them to one Host incarnation; the
remote adapter maps them to authenticated Worker identity/incarnation without
placing Worker protocol vocabulary in the Session aggregate.

`SessionRealizationLease`, not a transient Run claim, owns the continuing
projection of a Session environment and its MCP routes. A Run claim authorizes
the claim-time application contribution only. The attachment lifecycle may
outlive that Run, so every stage/publish/drain request carries the exact Session
lease epoch and Runtime incarnation and fails closed after replacement or expiry.
The root mutation that enters `Realizing` records `McpRealizationClaim` under the
current Session lease before external I/O.

The application layer sees one `McpAttachmentRealizer` port owned by
`awaken-session-contract`. `SessionRuntime` retains ordinary Session execution
and preparation only; it does not duplicate MCP stage/publish/drain. A local
Runtime Host implementation realizes the projection in-process. A downstream
implementation can be injected through `WorkerNodeBuilder` and realizes the
same exact-generation request through its gateway. These are topology adapters
over one port, not two realization models. The Worker has no direct Control
database access and cannot mutate desired state.

Local Managed execution and remote Worker execution also reuse one exported
`drive_session_realization` phase driver. Topology adapters implement only
`SessionProjectionSynchronizer` and the existing realization/control ports; they
cannot copy the Stage → Activate → Publish/Drain → Acknowledge ordering, receipt
verification, partial-stage cleanup, or failure reporting. The Worker
application-control client extends `SessionRealizationControl` instead of
redeclaring parallel activate/acknowledge/fail methods.

The Managed Session application layer persists intent and checks authorization.
`stage_mcp_attachment` creates an exact-generation route/connection that remains
invisible to Runtime tools and rejects external calls until durable activation.
After the same generation is CAS-committed `Active`,
`publish_mcp_generation` makes it visible at the Runtime's safe boundary.
`drain_mcp_generation` first rejects new calls and then closes the exact
generation idempotently. A failed/stale activation CAS disposes its staged
receipt; it never publishes it.

For a remote Worker, `FrozenSessionProjection.mcp` is durable realization input,
not a reason to reject or install a second desired-state copy. The Worker first
installs only the frozen baseline, Environment, and Resource projection, then
executes the directive's exact `mcp_stages` through `McpAttachmentRealizer`.
Only the subsequent Control activation directive permits publish. A failed
stage reports `fail_session_realization`, drains any earlier receipts from that
batch, and does not open the Session environment; there is no local retry or
credential fallback.

Runtime Host implements the port by adapting the private existing `McpRelay`,
`awaken-ext-mcp` connection behavior, and Runtime safe-boundary capability
refresh. A process-local MCP runtime is only a projection of durable active
attachments and never a second desired-state registry.

For an authenticated ACP attachment, `stage_mcp_attachment` is the only route
creation boundary. Publication may expose or renew that exact staged route and
drain may revoke it; constructing or rebuilding a Session runtime is a
read-only projection step and must neither start the relay nor synthesize a
missing route. After process loss, the durable realization recovery protocol
must replay the same exact stage and publish phases before Runtime construction.
This prevents runtime reads from becoming a hidden second realization path.

The same rule applies to Run continuation reads. A cold `pending` projection
opens only committed Run truth; its `AwaitReason` is the authoritative
classification (`ExternalEvent` expects a client result, `ToolPermission`
expects an allow/deny decision). It must not rebuild a Runtime context or reopen
an Environment merely to rediscover that fact from a tool catalog. The first
post-restart driving event instead enters the canonical Session admission,
advances the realization lease/incarnation, and only then opens the Session's
Running interval and resumes the committed Run. Ordinary messages, permission
confirmations, and client-tool results share that one ordering.

Every relay route, Runtime descriptor, tool call, receipt, and drain request
carries `(session_id, attachment_id, generation)`. Replacement receives a new
route identity; an old route never starts using a new generation's credential.
Each external effect rechecks generation and the exact Session lease epoch.
Continuing ownership is explicitly renewed through the same phase protocol.
`renew_existing_lease` is false for create and hot-update work and true only for
the lease supervisor, so a later wall clock cannot accidentally restage every
unchanged attachment. A valid renewal retains attachment generation,
realization id, owner incarnation, epoch, target, credential pin, and selected
holder; it advances only expiry and the stage-attempt idempotency key, then
requires an exact Runtime receipt and publication acknowledgement. No
relay-private lease registry or renewal API exists.

`awaken-provisioning-contract` continues to own Sandbox environment, process
launch, `NetworkPolicy`, env/mount delivery, and `SandboxProvider`. It does not
own MCP target selection, transport streams, route lifetime, or hot-plug state.

`McpAttachmentRealizer` is the only public realization abstraction. An injected
implementation is exclusive: an error is terminal for that exact attempt and
must never fall back to the local Host relay, because fallback would change the
selected custody boundary. `McpRelay` remains `pub(crate)` and is not visible to
Session, Worker assembly, or downstream implementations. Awaken never depends
on downstream gateway, IAM, route, or Vault-backend types.

### D6: one frozen network authority constrains every generation

`SessionBaseline.environment.network: NetworkPolicy` is authoritative for the
Session. Managed networking, legacy Sandbox networking, and `deny_egress` are
compatibility inputs normalized once by safe intersection and then discarded as
authorities.

A hot attachment may use a route behind a stable endpoint already admitted by
the frozen policy. It may not widen the Sandbox allowlist. Direct access to a new
host outside the policy requires an explicit Environment migration and normally
a replacement Sandbox.

The implemented projection has no `ThreadEgress` or `ThreadSandbox` mutation
surface. `SessionInit` carries the exact `EnvironmentSnapshot`; Native and ACP
use the same realized, Session-owned `SessionEnvironment`. Empty
allowlists canonicalize to `None`, and a retained `sandbox.network` field is
discarded before provider realization, so it cannot widen or replace the frozen
network fact.

### D7: Runtime refresh is reused, not duplicated

The existing Runtime live-version and safe-step refresh mechanism remains the
only capability refresh path. Runtime Host projects active MCP generations into
the existing Session-supplied runtime capability surface.

`McpManager` may retain reusable per-server connection and tool-discovery
behavior, but it stops being a Session-level desired-state registry. No public
type uses Runtime extension or plugin vocabulary to name the Session domain.

Native and ACP consume the same durable attachment state. An ACP backend that
cannot hot-swap MCP must declare that limitation and reject the command; it may
not report false success, silently restart, or retain a removed credential.

## Static Structure

```text
Control creation inputs ───────────────┐
                                      ▼
                     SessionCreationIntent (Preparing)
                                      ▲
claim-fenced application contribution┘
                                      │ one compiler / one root CAS
                                      ▼
                               Session aggregate
                               ├── root revision
                               ├── frozen SessionBaseline
                               │   └── exact EnvironmentSnapshot
                               ├── SessionResourceState generation 1
                               └── SessionMcpAttachmentSet generation 1
                                      │
Published/inline/application MCP ──────┘ one normalizer
                                                   │
                                                   │ one McpAttachmentRealizer port
                                                   ▼
                                 SessionRealizationLease
                                         │
                                         ├── local Runtime Host adapter
                                         └── remote Worker adapter
                                                   │
                                                   ▼
                                         Runtime Host projection
                                         ├── SessionEnvironment/Sandbox
                                         ├── existing MCP relay/transport
                                         └── Runtime safe-boundary refresh
```

## Dynamic Behavior

The durable transition table is authoritative:

| Current | Trigger | Next | Visibility / external action |
|---|---|---|---|
| absent | authorized add/create CAS | `Requested` | invisible; no network I/O |
| `Requested` | realization claim CAS | `Realizing` | invisible; stage exact generation |
| `Realizing` | successful receipt + activation CAS | `Active`, unacknowledged | publish only after CAS |
| `Active`, unacknowledged | exact publish acknowledgement CAS | `Active`, acknowledged | response/event may project success |
| `Realizing` | stage failure or stale activation CAS | `Failed` | dispose staged result; invisible |
| `Active` | remove/replacement switch CAS | `Draining` | hide and reject new calls first |
| `Draining` | drain/cleanup acknowledged + CAS | `Removed` | release route, credential, stream, lease |

No other transition is valid. `Failed` and `Removed` are terminal; retrying a
failed logical command allocates a new generation and never resurrects the
failed one. External I/O begins only after `Realizing` is durable, and tool
visibility begins only after `Active` is durable. A successful Managed response
requires the exact publication acknowledgement to be durable as well.

### Initial Session creation

1. The Managed edge resolves the exact Agent and Environment revisions.
2. One repository transaction persists the owner fence, root revision,
   `Preparing(SessionCreationIntent)`, and lifecycle intent without external
   realization. A no-application composition records `Absent`.
3. After a remote claim, the application produces one secret-free contribution.
   The Worker submits it to Control; exact claim/epoch and plan fingerprint are
   verified. Re-delivery replays and a conflicting contribution fails closed.
4. After the required contribution is committed or declared absent, one compiler
   produces `CompiledSessionCreation`. One root CAS consumes the intent, freezes
   `SessionBaseline`, and writes Resource/MCP generation 1 state.
5. A later root CAS acquires or verifies `SessionRealizationLease` and moves
   each required MCP generation from `Requested` to `Realizing`; Host then
   creates or adopts the Sandbox and stages required
   Resources and exact-generation MCP routes without exposing them.
6. Host checks the current Session lease epoch before and after each external
   realization.
7. One root-revision CAS commits the successful Resource/MCP generations
   `Active`; Host publishes MCP tools only after that commit, and the Session
   becomes idle after all required projections acknowledge publication.
8. A stale activation CAS or expired Session lease disposes the staged
   realization. Any required initial failure aborts activation and disposes partial results;
   durable nonterminal state remains recoverable.

The preparation branch is tested from this causal graph:

```text
create(required) -> Preparing --valid contribution--> Frozen -> realization -> idle
                              \--delete-------------> tombstone / not found
                              \--no command---------> remains inert
```

| Preparing input | Terminal command | Runtime preparation | Result |
|---|---|---|---|
| absent | none | never | remains `preparing` |
| valid exact contribution | none | once after the finalization CAS | `idle` after acknowledgement |
| conflicting/replayed contribution | none | never/never again | conflict/exact replay |
| absent | delete | never | tombstone and public not found |
| contribution after delete | already deleted | never | not found; no resurrection |

### Managed MCP full replacement

1. The Managed update edge authorizes the existing Session operation and parses
   `agent.mcp_servers`; it never mutates `SessionRecord` or an event projection.
2. The sole normalizer produces the canonical desired-set fingerprint and exact
   drafts. A matching active fingerprint is a no-op; a matching nonterminal
   operation is adopted/replayed.
3. The aggregate diffs current logical attachments into internal add, replace,
   remove, and unchanged decisions and root-CAS persists all requested intent
   before external I/O.
4. The realization coordinator drives the generation transitions below under
   the current Session lease. Replacement keeps the old active generation until
   the new one stages successfully; failure leaves the last visible set intact.
5. The response and `session.updated` event are projected only from the
   durably active set after Runtime publication acknowledges the same
   generations. A failed command returns a typed error and cannot report the
   uncommitted desired set as active.

### MCP add

1. The command carries an idempotency key and expected Session revision.
2. The Managed adapter authorizes and normalizes one exact attachment draft.
3. The aggregate allocates a new generation and CAS-persists `Requested` before
   network I/O.
4. A second CAS claims that generation as `Realizing` under the current
   `SessionRealizationLease`; Host verifies the lease and stages its
   route/connection without making it callable.
5. Host verifies the lease epoch again and CAS-activates the same generation. A
   stale CAS disposes the staged receipt.
6. Runtime publishes tools at its next safe boundary. Publication failure leaves
   durable `Active` intent non-visible and retryable by reconciliation; it never
   authorizes a different generation or credential.

A failed add becomes `Failed`, exposes no tool or route, and does not disturb
other active attachments.

### MCP replace

1. A replacement gets a new generation while the old generation remains active.
2. CAS moves the new generation through `Requested` to `Realizing`; Host stages
   and validates the replacement without making it visible.
3. One CAS makes the new generation active and the old one `Draining`.
4. Runtime publishes the new exact-generation descriptors and hides the old
   generation at one safe boundary. The old route accepts no new calls, drains or
   times out in-flight work, then releases credential material, connection, and
   lease.
5. A failed or stale replacement is disposed without changing the old active
   generation.

### MCP remove

1. CAS moves the active generation to `Draining`.
2. Runtime hides its tools and rejects new exact-generation calls.
3. In-flight calls and streams drain to a deadline.
4. Route, connection, credential material, and lease are released idempotently.
5. CAS records `Removed`.

### Recovery and ownership loss

Recovery scans nonterminal attachments from the Session repository. It recreates
process-local routes and connections only after validating Session revision,
attachment generation, `McpRealizationClaim`, and `SessionRealizationLease`.
Plaintext and live handles are
never deserialized.

`Requested` is safe to claim under the current Session lease. `Realizing` with a
matching unexpired realization id, Runtime incarnation, and lease epoch adopts or
continues the staged receipt; an expired owner is fenced and the exact generation
may be restaged under a new realization id and epoch. Durable `Active` without a
published Host projection is republished idempotently, and `Draining` is never
made visible again. A receipt whose activation CAS lost is an orphan and is
disposed. These are the only crash recovery interpretations.

Session lease replacement/expiry, revocation, or revision mismatch hides the
attachment and rejects new effects before cleanup. Cleanup retries cannot restore
visibility or select a weaker credential realization.

### Domain-owned external-effect protocols

Session realization deliberately does not introduce one universal effect or
transaction facade. Each bounded context retains its own command and receipt
vocabulary while following the same causal order:

1. Environment create/adopt emits a `SessionEnvironmentReceipt` under the exact
   realization lease before a live binding is published.
2. MCP stages with `McpRealizationReceipt`; the safe-boundary publish and drain
   projections return and verify `McpProjectionReceipt` for the exact generation.
3. Terminal archive/delete/recovery commits `SessionTerminalCleanupState::Requested`,
   durably retains every root/child Runtime identity, invokes the same per-thread
   cleanup intent on every retry, and commits `Completed` only after Repository,
   Skill, and artifact settlement plus Environment disposal. Resource settlement
   and removal of the authoritative Environment binding share that root CAS.
4. Artifact harvest authors `(effect_id, content_id)` while the Sandbox and claim
   are still live. Resources verifies the bytes and idempotency identity, returns
   `ArtifactPublicationReceipt`, and Runtime verifies it before disposal.

This keeps the design small: the shared rule is an invariant and execution order,
not a god interface. Environment owns bindings, Session owns terminal intent,
MCP owns generations, and Resources owns File records. Fresh execution and
recovery reuse the same domain-specific entry points.

The local composition runs one Managed realization-lease supervisor. A remote
Worker couples renewal to its authenticated registry heartbeat; Control caps the
requested Session expiry by the current Worker registry lease. If heartbeat,
registry mutation, or due renewal can no longer prove authority, the Worker
stops claiming and disposes every process-local Session environment and MCP
projection through the terminal Host path. Route capabilities and credential
material are therefore removed rather than surviving as an independent relay
lifecycle.

## Ownership and Dependency Rules

| Owner | Owns | Must not own |
|---|---|---|
| Managed/config | Environment definitions, publication, compatibility input validation | live Sandbox, MCP generation, plaintext |
| Session contract | baseline, root revision, Resource/MCP versioned state, repository port | relay, gateway, Vault selection at runtime |
| Session store | SQLite/Postgres atomic CAS, idempotency, recovery queries | domain selection or live connection |
| Managed Session application | authorization edge, one normalization, full-replacement diff, claim-fenced application contribution, command orchestration | second attachment registry or plaintext persistence |
| Session realization coordinator | Session lease/incarnation/epoch and exact-generation effect orchestration | Run business state, credential selection, or live route implementation |
| Runtime Host / Worker adapter | realization projection, Session-lease checks, relay/connection lifecycle | durable desired truth, direct Control database access, or repeated credential selection |
| Provisioning contract/provider | Sandbox/process/network/env/mount enforcement | MCP route, stream, credential choice, attachment lifecycle |
| Runtime Core | existing safe-boundary capability refresh and tool execution | Session persistence, Vault, Managed Environment, MCP authoring |
| Downstream adapter | hosted gateway/lease behavior behind a future Session-owned port | reverse dependency or changes to Awaken domain types |

## Implemented Consolidation and Continuing Proof

The following list is retained as the implementation/deletion ledger. Its
authoritative paths have landed; it is no longer a pre-coding gate. Remaining
work is evidence for deployment-specific cells, not permission to reintroduce
the removed authorities.

1. align the aggregate with ADR-0051's scoped persistence envelope and define
   Environment revision/snapshot compilation;
2. close root Session delete/tombstone/idempotency semantics and add repository
   CAS for SQLite/Postgres;
3. migrate Resource, environment, lifecycle, archive, and delete mutations from
   direct/process-local serialization to the same CAS;
4. define claim-fenced application contribution and Session realization lease
   protocols for local and remote adapters;
5. introduce one MCP attachment state, one normalization path, and one canonical
   Managed full-replacement command;
6. route initial and hot MCP through that state and the existing relay/transport;
7. migrate and delete `SessionInit.mcp_servers`,
   `ApplicationSessionPlan.mcp_servers`, `PreparedMcpServer`, and parallel Host
   merge helpers;
8. delete the wire-only `ManagedState::update_session(..., mcp_servers)` state
   assignment after it delegates to the aggregate command;
9. retain `McpManager` only for reusable connection behavior, not Session truth;
10. normalize networking once into the baseline `NetworkPolicy` and remove later
   precedence decisions;
11. prove Native/ACP consume the same active attachment generation.

The migration is complete only when every old authority has the following
replacement and deletion proof:

| Old authority/path | Replacement | Required migration proof | Delete when |
|---|---|---|---|
| `SessionInit.mcp_servers` | generation-1 `SessionMcpAttachmentSet` | Managed create/recovery use exact drafts | all create/recovery tests use attachments |
| `ApplicationSessionPlan.mcp_servers` | secret-free `mcp_inputs` through the normalizer | registered applications preserve initial MCP behavior | every application caller stops constructing `PreparedMcpServer` |
| Worker-local application-plan installation | claim-fenced `ApplicationSessionContribution` into Control root CAS before activation | replay/conflict/stale-claim and no-application tests | Worker slots contain no application baseline or MCP desired state |
| `ManagedState::update_session(..., mcp_servers)` wire-only assignment | canonical full-replacement command, normalizer, and aggregate diff | SDK update changes actual Runtime tools/routes and event derives from active state | no code mutates the wire MCP projection directly |
| `PreparedMcpServer` | `McpAttachmentDraft` plus secret-free realization receipt | bearer opens only in the selected realization boundary | no public/Host field carries a prepared bearer |
| Host `thread_session_mcp` and merge helpers | durable visible-generation projection | Native and ACP read the same projection | no process-local merge remains |
| `SessionRuntimeSlot` MCP desired state | Session repository state | slot contains live handles only | recovery reconstructs it from durable generations |
| `McpManager` Session registry | attachment set | per-server transport/discovery retained independently | it cannot add/remove/select Session servers |
| relay `(thread, name)` route | `(session, attachment, generation)` route/capability | stale call and replacement tests | old URL cannot reach a new credential |
| Run-claim ownership of continuing Session projection | `SessionRealizationLease` plus per-generation `McpRealizationClaim` | Worker replacement, lease expiry, orphan adoption/disposal | attachment effects no longer depend on an expired Run claim |
| `ThreadEgress`, late `deny_egress` precedence | baseline `NetworkPolicy` | safe-meet and provider projection tests | Host has no later policy choice |
| `PersistedSession.runtime.mcp_servers` and stored wire echo | attachment-derived projection | wire compatibility fixtures | only the attachment set is persisted as MCP truth |
| process-local Resource mutation mutex/direct Session updates | root mutation CAS | SQLite/Postgres multi-process conflicts | every aggregate write uses expected revision |

## Implementation Slices

Slices 0-3 have landed in the repository and are retained here to document the
causal implementation order and the evidence expected when those paths change.

### Slice 0: contract closure gate

- remove `workspace_id` from the Session aggregate and retain the ADR-0051 scope
  envelope;
- fix Environment revision, snapshot, fingerprint, and realization-profile
  ownership;
- fix application contribution timing and Worker-to-Coordinator claim fencing;
- fix Session realization lease, per-generation claim, and remote command/receipt
  semantics;
- fix root delete/tombstone/idempotency and public full-replacement CAS mapping;
- include the existing wire-only MCP update in the mandatory deletion map;
- coordinate ADR-0067's attempt binding, sealed-envelope, and OAuth refresh
  contracts.

This gate was satisfied before the production paths in Slices 1-3 landed. Slice
0 remains contract history and a regression constraint, not a temporary
implementation layer or an outstanding prerequisite.

### Slice 1: convergence before hot plug

- root Session mutation CAS, idempotency/outbox atomicity, and store conformance;
- migrate Resource/environment/lifecycle writes off direct Session updates;
- consumed preparation intent, immutable finalized baseline, and generation-1
  Resource/MCP state;
- one MCP normalizer for published, compatibility, and application inputs;
- existing relay/transport/runtime refresh reused;
- superseded initial MCP authorities deleted;
- Rust integration and TypeScript create/call/secret-nonleak coverage green.

### Slice 2: add and remove

- route the existing Managed `agent.mcp_servers` full replacement through the
  one normalizer and aggregate diff; delete its wire-only assignment;
- idempotent commands and exact-generation fencing;
- durable Requested/Realizing/Active/Draining/Removed transitions;
- stage/activate/publish compensation and crash recovery;
- safe-boundary tool refresh and stale-call rejection;
- single-Worker Native E2E, failure and ownership-loss coverage.

### Slice 3: replace, recovery, and ACP parity

- atomic generation switch, drain, crash reconciliation, orphan expiry;
- SQLite/Postgres multi-process conflict tests;
- ACP hot-swap conformance or explicit capability rejection;
- TypeScript add/call/replace/remove/restart E2E.

Generic Service authoring, dynamic HTTP/WebSocket service attachments, general
Git service attachments, public generic-realizer injection, and downstream
platform custody are explicitly deferred. Existing Repository Git activation
remains a Resource lifecycle and is not deferred by this decision. Each deferred
item requires a concrete second implementation and a separate accepted slice.

## Consequences

- Immutable facts and dynamic relations no longer share one specification.
- Initial and hot MCP use one durable authority instead of an overlay.
- Resource and MCP concurrency share one Session commit boundary.
- Model and Repository retain their existing authoritative aggregates.
- Existing relay, transport, Sandbox, and Runtime refresh mechanisms are reused.
- The first implementation removes paths before adding protocol breadth.

## Rejected Alternatives

- **Keep services inside an immutable provisioning spec.** It makes hot mutation
  either modify frozen state or create an overlay.
- **Persist an inference Service attachment.** It duplicates
  `ResolvedModelCandidate`.
- **Create a generic Service domain before MCP works.** It merges unrelated
  domains and adds unproved abstractions.
- **Place a service realizer in `awaken-provisioning-contract`.** MCP routes and
  streams exceed its Sandbox/process boundary.
- **Add an MCP repository.** The Session aggregate and repository already own
  dynamic Session state.
- **Use a process-local mutex as the consistency boundary.** It cannot protect
  multiple Control processes.
- **Maintain old and new MCP paths with dual writes.** Synchronization is not a
  substitute for one authority.

## Development Readiness and Implementation Gate

The contract-closure gate and Slices 1-3 are implemented: the scoped aggregate,
exact Environment snapshot, claim-fenced transport envelope plus claim-free
domain command, root CAS/tombstone/idempotency behavior, one attachment
normalizer, and one `McpAttachmentRealizer` driven by
`drive_session_realization` are the current code paths. Rust store/runtime tests
and the Managed TypeScript MCP matrix are the regression gate for further
development.

Implementations may refine private helper names but may not introduce another
durable MCP registry, credential selector, network-policy authority, public MCP
mutation surface, or long-lived dual-write path. G42 remains listed as a target
guardrail until all deployment-specific Native/ACP and ownership-loss evidence
is continuously enforced, not because feature coding is still blocked.

## Amendment: HTTP audit identity does not own domain replay (2026-08-12)

The Session root aggregate remains the sole owner of `Idempotency-Key` replay,
payload-conflict, revision, event, and response semantics for the existing Managed
full-replacement command. The outer durable management-audit edge records the
HTTP attempt under an explicit `X-Request-ID`, or a generated attempt identity
when that header is absent. It must not reuse `Idempotency-Key` as its audit-call
identity: doing so creates a second dedupe authority that can reject an exact
domain replay before the Session handler returns its committed projection.

An explicit repeated `X-Request-ID` still fails closed at the audit boundary, and
a conflicting reuse remains storage corruption/identity conflict. A repeated
domain `Idempotency-Key` with no repeated audit request id reaches the canonical
Session command, which returns the existing committed revision without another
generation, Runtime effect, or `session.updated` event.
