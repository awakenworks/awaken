# ADR-0067: Credential Plaintext Boundaries and Model Exposure

- Status: Accepted
- Date: 2026-07-24
- Accepted: 2026-07-25
- Builds on: [ADR-0062](0062-published-inference-access-and-runtime-credential-injection.md)
  (one exact published model candidate and credential access pin)
- Coordinates with:
  [ADR-0066](0066-session-service-binding-and-realization.md)
  (Session baseline and MCP attachment lifecycle), and
  [ADR-0063](0063-resource-input-identity-configuration-pinning-and-lifecycle.md)
  (Repository configuration pinning and activation)
- Clarifies [ADR-0043](0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md):
  Vault storage at rest, material location, execution-time plaintext permission,
  model exposure, and realization mechanism are separate facts
- Detailed design: [Credentials and Vaults](../design/credentials-and-vaults.md)
- Target guardrail: G43

## Context

Awaken already has several credential concepts:

- `CredentialSource` and `SecretStore` own identity, revision, status, and sealed
  material at rest;
- `CredentialAccess` freezes an exact `CredentialRef`,
  `CredentialInjectionKind`, and `CredentialUsage` in a published snapshot;
- `PinnedCredentialMaterializer` verifies exact scope, revision, status,
  provider, and usage before opening material;
- `EnvValue::Secret`, `MountSource::Secret`, `SecretBroker`, and Sandbox
  capabilities express last-mile delivery primitives;
- the Worker-local MCP relay can hold a bearer outside a Sandbox;
- a downstream deployment may keep material behind its own gateway and opaque
  lease.

The previous form of this ADR added a second `CredentialDelivery` vocabulary and
ordered custody as:

```text
WorkloadHeld < WorkerHeld < PlatformHeld
```

That order is not generally valid. A local Worker and a downstream platform are
different trust domains; one is not automatically authorized merely because it
is called stronger. The previous `CredentialDelivery` also overlapped existing
`CredentialInjectionKind` and `CredentialUsage`, while `PlaintextAllowed` added a
model-visible secret mode without an approved use case.

The same document also projected `ResolvedModelCandidate` into a persisted
Session Service attachment and included automatic LLM Vault authoring. The first
would duplicate ADR-0062's sole model authority. The second is a separate
multi-repository authoring workflow whose application service, transaction,
idempotency, compensation, and outbox have not yet been designed.

## Decision

### D1: five credential facts remain distinct

The architecture separates:

| Fact | Meaning | Authority |
|---|---|---|
| storage at rest | where sealed material is persisted | Credential/Vault context |
| material source and envelope | which trusted adapter resolves a reference and whether material crosses in a recipient-bound sealed form | published `CredentialAccess` |
| usage | how the target consumes authentication | existing `CredentialUsage` |
| plaintext permission | exact trust domains allowed to hold real material | published execution policy |
| model exposure | whether model-visible content may contain no credential or only a virtual value | published execution policy |

Authorization remains a separate front-door decision. None of these facts grants
access by itself.

### D2: explicit allowed plaintext holders replace a custody floor

The target policy is:

```rust
struct CredentialExecutionPolicy {
    allowed_plaintext_holders: BTreeSet<PlaintextHolder>,
    model_exposure: ModelExposurePolicy,
}

struct PlaintextHolder {
    boundary: PlaintextBoundary,
    trust_domain: TrustDomainRef,
}

enum PlaintextBoundary {
    Workload,
    Worker,
    Platform,
}

struct TrustDomainRef(String);

struct CredentialRealizationProfile {
    inference_holder: PlaintextHolder,
    mcp_holder: PlaintextHolder,
    resource_holder: PlaintextHolder,
}
```

`TrustDomainRef` is an opaque, neutral identifier. It does not import hosted IAM,
gateway, role, or principal vocabulary. The allowed set is validated as nonempty
for protected access.

Admission selects one exact holder:

```text
selected_plaintext_holder ∈ allowed_plaintext_holders
```

There is no ordering and no `realized >= floor` rule. Selection is not an
implicit iteration over the set: trusted Environment/deployment normalization
produces one `CredentialRealizationProfile`, frozen into the Session baseline and
dispatch attempt execution plan. Its purpose-specific holder is exact, and admission
validates that it is both allowed by the published policy and supported by the
installed adapter and provider. Zero matches fail unsupported; multiple possible
matches do not matter because no runtime preference algorithm runs. Changing the
requested holder is a new Session MCP/Resource generation or Run attempt, never
a failure fallback.

Unauthenticated access has no credential and therefore no plaintext-holder
policy.

### D3: model exposure initially has only two states

```rust
enum ModelExposurePolicy {
    Forbidden,
    VirtualOnly,
}
```

- `Forbidden` allows neither real nor virtual credential representations in
  prompts, model context, model-visible Resource instructions, or tool
  arguments.
- `VirtualOnly` allows a synthetic placeholder or scoped capability that looks
  credential-like but cannot reveal or derive the backing material. It must be
  bound to target, Session/Run, generation, trust domain, and expiry.

Real material in a process environment does not imply model exposure. An ACP
process can be an allowed Workload plaintext holder while model exposure remains
`Forbidden`.

`PlaintextAllowed` is rejected until a concrete use case, authorization rule,
audit contract, redaction behavior, and security review exist.

### D4: extend the existing access contract instead of adding delivery policy

The published access value separates material source from an optional sealed
payload reference and transport binding:

```rust
struct CredentialAccess {
    credential: CredentialRef,
    material_source: CredentialMaterialSource,
    envelope: Option<CredentialEnvelope>,
    usage: CredentialUsage,
    refresh: Option<CredentialRefreshAccess>,
    policy: CredentialExecutionPolicy,
}

struct CredentialRefreshAccess {
    credential_revision: u64,
    configuration_fingerprint: Fingerprint,
    token_endpoint: Url,
    client_id: String,
    token_endpoint_auth: TokenEndpointAuth,
    client_secret_ref: Option<SecretMaterialRef>,
    refresh_token_ref: SecretMaterialRef,
    access_token_ref: SecretMaterialRef,
    scope: Option<String>,
    resource: Option<String>,
}

enum CredentialMaterialSource {
    ControlPlaneReference,
    WorkerReference,
}

enum CredentialEnvelope {
    SealedForWorker {
        envelope_ref: SealedCredentialEnvelopeRef,
        recipient: TrustDomainRef,
        expires_at: Timestamp,
    },
    SealedForWorkload {
        envelope_ref: SealedCredentialEnvelopeRef,
        recipient: TrustDomainRef,
        expires_at: Timestamp,
    },
}

struct SealedCredentialEnvelopeRef {
    id: String,
    payload_fingerprint: Fingerprint,
}

struct CredentialMaterialBinding {
    workspace_id: String,
    target_use_fingerprint: Fingerprint,
}

struct CredentialMaterialRequest<'a> {
    access: &'a CredentialAccess,
    selected_holder: &'a PlaintextHolder,
    binding: &'a CredentialMaterialBinding,
}

trait CredentialMaterialResolver {
    fn supported_material_sources(&self) -> Set<CredentialMaterialSource>;
    fn supports_recipient_bound_envelopes(&self) -> bool;

    async fn resolve_exact(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError>;
}
```

This refines the existing `CredentialInjectionKind` rather than adding a parallel
`CredentialDelivery` decision:

| Existing value | Target interpretation |
|---|---|
| `Reference` | `material_source = ControlPlaneReference`, no envelope |
| `WorkerReference` | `material_source = WorkerReference`, no envelope |
| `SealedEnvelope` | explicit source plus recipient-bound `CredentialEnvelope` |
| `Direct` | compatibility decode only; rejected for new secret-free publication |

Legacy `Direct` provenance is durable compatibility state, not a transient decode
hint. Serialization and queue/database round trips preserve it so a legacy value
cannot be washed into an apparently valid unsealed Control reference before
admission. Every later claim and execution boundary continues to reject it.

`CredentialUsage` remains authoritative for `ProviderAdapter`, HTTP header/query,
typed HTTP Basic, client certificate, environment variable, file semantics, and
one built-in platform-held HTTP effect. The HTTP-effect usage freezes every
material field together with every exact destination at which that field may be
rendered:

```rust
enum HttpEffectPlacement {
    Header { name: String },
    Query { name: String },
    JsonPointer { pointer: String },
}

CredentialUsage::HttpEffect {
    fields: BTreeMap<String, BTreeSet<HttpEffectPlacement>>,
}
```

Header and query names are nonempty effect destinations. JSON destinations are
canonical RFC 6901 pointers rooted at the effect's `json` or `body` value; the
empty pointer denotes that complete value. A single opaque secret is admissible
only for exactly one declared field. Structured material is admissible only when
its complete field set equals the declared field set. OAuth material is never an
HTTP-effect material. Empty fields/placements, malformed pointers, undeclared
material fields, and broader or narrower effect-reference sets fail before the
Gateway performs I/O. This is a built-in `PlatformRelay` usage, not an
`Extension` consumer or plaintext-returning RPC.

MCP, Model, and Resource adapters must not invent protocol-specific
credential-usage fields.

A Repository configuration remains authoritative for its Vault source binding.
Before the resolved input enters the Session aggregate, the Session application
compiles that binding into one `ResolvedRepositoryCredential`: exact
`CredentialAccess`, canonical `HttpBasicAuth` usage, `Forbidden` model exposure,
and the Environment profile's exact `resource_holder`. The Vault material is the
typed `awaken.http-basic/v1` document with `username` and `password` fields.
Runtime receives this pin
instead of a bare source id. It validates the binding, revision, usage, policy,
and holder before using the same `CredentialMaterialResolver` as Model and MCP.
This reuses credential execution mechanics without turning Repository into an
MCP attachment or a generic Service aggregate.

`CredentialRefreshAccess` preserves the existing MCP OAuth refresh/reseal
capability as an exact, secret-free credential execution fact. It is compiled
from the same credential revision as `credential`; its fingerprint covers token
endpoint, client authentication, scope/resource, and all opaque material
references. Runtime may open only those exact references, rotate/reseal the
access and refresh tokens through the credential-owned material store, and retry
the challenged request. It may not rediscover refresh configuration by MCP URL,
scan current Vault contents, or silently adopt a newer credential revision.

An envelope reference resolves one opaque sealed payload; the published snapshot
does not contain plaintext. The resolver validates its payload fingerprint and
replay binding to the exact credential revision, recipient trust domain,
target/use fingerprint, and expiry before opening or forwarding it. Only that
recipient may open it. The envelope does not authorize the recipient: the same
holder must appear in the allowed set and be selected by the execution pin.

`CredentialMaterialBinding` is computed at the authoritative consumer edge from
the trusted Workspace plus the canonical Model endpoint, MCP target, or
Repository id/config together with the existing `CredentialUsage`. This closes
the target/replay information gap without putting Model, MCP, or Repository
types in the Credential context. Admission additionally requires explicit
recipient-bound-envelope capability evidence; a material source alone cannot
claim that the installed adapter can validate sealed claims.

`CredentialMaterialResolver` is the sole neutral material-source port. A Control
reference adapter, Worker-private adapter, or recipient-bound envelope adapter
may implement it, but all consume the same exact access and holder selection.
The port cannot enumerate credentials, choose another revision or holder, or
return material to a boundary different from the selected holder. Hosted Vault,
IAM, gateway, and transport types remain outside the contract.

The self-hosted `PinnedCredentialMaterializer` deterministically handles an
unsealed Control reference locally and delegates a Worker reference or any
envelope to at most one explicitly installed resolver. A failed delegate never
falls back to the local Vault. Worker assembly reuses this same materializer for
Native inference, ACP process secrets, MCP, and Repository realization.

The realization mechanism is an execution result, not another published policy.
A secret-free receipt may record the selected holder and actual mechanism, such
as process secret, secret file, Worker relay, or platform lease. It cannot select
a credential or authorize a holder.

### D5: Model remains outside Session MCP state

`ResolvedModelCandidate` remains the sole published model selection and access
authority:

```text
ResolvedModelCandidate
        │ exact candidate only
        ▼
PinnedCredentialMaterializer / alternate installed materializer
        ├── Native executor binding
        └── ACP process or mediated endpoint binding
```

No inference `ServiceSnapshot` or `SessionServiceAttachment` is persisted. If a
mediated implementation needs a request value, it is transient and references
the candidate:

```rust
struct InferenceRealizationRequest<'a> {
    candidate: &'a ResolvedModelCandidate,
    environment: &'a EnvironmentSnapshot,
}
```

It performs no catalog lookup, route selection, model fallback, credential
selection, or persistence. Session `vault_ids` never select or override the
model credential.

The selected plaintext holder is an execution fact owned by one exact attempt
epoch. Durable work binds that epoch to its dispatch claim; an explicitly
non-durable in-process Run binds it to the currently active Session `run_id`:

```rust
struct AttemptCredentialBinding {
    candidate_fingerprint: CandidateFingerprint,
    credential: CredentialRef,
    selected_plaintext_holder: PlaintextHolder,
    selected_realization_kind: CredentialRealizationKind,
    claim_epoch: u64,
}

enum CredentialRealizationKind {
    ProcessSecretEnvironment,
    PrivateSecretFile,
    WorkerProviderAdapter,
    WorkerRelay,
    PlatformProviderAdapter,
    PlatformRelay,
}

struct CredentialRealizationReceipt {
    candidate_fingerprint: CandidateFingerprint,
    credential: CredentialRef,
    selected_plaintext_holder: PlaintextHolder,
    actual_realization_kind: CredentialRealizationKind,
    claim_epoch: u64,
    receipt_fingerprint: Fingerprint,
}
```

`selected_realization_kind` is an admission decision; the receipt records what
the materializer actually completed. The actual kind must equal the selected
kind or the effect fails and its material is disposed. They are not one
ambiguous field. The
binding is committed atomically with the dispatch claim's owner, monotonic epoch,
Worker incarnation, and lease before materialization. The dispatch repository is
the persistence boundary: the claim operation stores the binding in the claimed
attempt-epoch record and returns it in `Claimed`; `RuntimeRunContext` remains live
wiring and is not durable authority.

An explicitly non-durable direct Run has no recovery or reassignment contract,
so it cannot truthfully create a durable dispatch claim. At the same pre-I/O
boundary it invokes the same neutral binding compiler with the frozen Environment
holder and installed materializer capabilities, assigns a process-local monotonic
epoch, and fences ownership to the Session's exact active `run_id`. Clearing or
replacing that active run invalidates materialization and receipt recording. This
is a different consistency adapter around one binding algorithm, not an unbound
credential path; it never falls back when admission or ownership fails.

Retries and response-loss recovery under the same claim epoch reuse the binding
exactly. Reclaim advances the epoch and creates a new attempt-epoch binding only
after the newly selected Worker and frozen execution profile request one exact
allowed holder. Failure within an epoch never changes it. A stale epoch cannot
materialize or commit a receipt. Native and ACP projections for one attempt
consume the same binding.

Broad queue selection evaluates credential admission with the requester's exact
capability evidence before mutating a row. A credential-incompatible row is
skipped so it cannot poison later admissible work; policy-ranked broad selection
uses the same rule. An exact run claim still returns the admission error rather
than disguising an invalid named request as absence.

### D6: MCP persists only a derived selection for its generation

An exact MCP attachment carries its published/normalized `CredentialAccess` and,
before external I/O, records the frozen Environment/execution profile's selected
`PlaintextHolder`. Admission proves that exact holder is allowed by the
credential policy and supported by the installed adapter.

The selection belongs to that attachment generation. Replacement creates a new
generation and a new authorization/admission decision. A realization failure
never changes the selected holder or falls back to plaintext in another trust
domain.

The existing Worker MCP relay is the first mediated implementation. It may claim
Worker plaintext only when:

- the exact Worker trust domain is allowed;
- the relay restricts target, Session, attachment, and generation;
- Sandbox network enforcement prevents bypass to the original target;
- ownership, lease, replacement, removal, and termination revoke the route;
- secret-leak tests prove the Sandbox receives no real credential.

The first MCP slice accepts only the authoring compiler's canonical
`CredentialUsage::HttpHeader { name: Authorization, scheme: Bearer }`. Runtime
validates this persisted usage before materialization. Query parameters,
arbitrary headers, client certificates, provider adapters, environment variables,
and files fail as unsupported instead of being silently reinterpreted as a
bearer. Supporting another MCP authentication form requires extending the same
usage-driven adapter and its decision table, not adding protocol-local fields.

The local relay projects an opaque, randomly generated route capability rather
than a predictable `(session, attachment, generation)` URL. The private route
record binds that capability to one exact target and generation and caps it by
the Session realization lease. Every forwarded request rechecks both capability
and expiry; wrong, expired, replaced, removed, and terminal routes return the
same not-found result without attempting the upstream request. Exact restaging
of the same generation preserves the capability for idempotent recovery, while
a new generation receives a new value. The capability is live projection state,
never Session desired state, a receipt, an event, or a log field.

The route does not own its lease lifecycle. One canonical Session realization
driver renews the exact active generation under the same owner incarnation and
epoch, and publication refreshes the private route expiry while preserving its
capability. Target, credential pin, holder, or generation changes are rejected
as conflicts rather than treated as renewal. Local supervision and signed
Worker heartbeat invoke this same protocol. Unprovable Worker authority stops
claims and terminally disposes every local Session projection, including relay
routes and their material.

`EnvVisibility::EgressOnly` is only a delivery hint. It proves nothing unless the
selected provider supports substitution and the network policy prevents bypass.

### D7: provisioning executes selected last-mile requirements only

`awaken-provisioning-contract` continues to expose neutral process/Sandbox
primitives. A Sandbox provider may execute an already-selected process-secret or
secret-file requirement through `EnvValue::Secret`, `MountSource::Secret`, and
`SecretBroker`.

The public Worker composition installs a downstream implementation through the
existing `ContainerEnvironmentProvider` seam. Its `SandboxCapabilities`,
create/adopt lifecycle, and broker installation are reused by the standard
manifest and Runtime Host; no credential-specific provider port is introduced.
`secret_egress_substitution && enforced_network_allowlist` is one shared
conjunctive custody predicate. Either fact alone is insufficient, and the
standard manifest must not publish `WorkerRelay` evidence without both plus an
installed exact credential materializer.

It must not enumerate credentials, select a revision, authorize a trust domain,
select an MCP target, or own relay/gateway lifecycle. Network realization remains
`NetworkPolicy` enforcement, not credential policy authorship.

The initial implementation matrix is closed:

| Selected holder | Native inference | ACP | MCP | Required evidence |
|---|---|---|---|---|
| Workload | not used for in-process Native provider access | typed process-secret or private secret-file requirement | only an explicitly trusted workload MCP client | last-mile delivery, process/file scope, cleanup, no base-env persistence |
| Worker | exact Worker provider adapter | Worker-mediated endpoint; no real secret in ACP workload | generation-fenced Worker MCP relay | exact substitution, no-bypass network, ownership/lease revoke |
| Platform | supported only through the accepted neutral `PlatformProviderAdapter` contract and an explicitly installed downstream adapter | deferred | deferred | exact Platform holder/capability evidence, claim fence, and secret-free receipt; other cells require a separate accepted downstream-adapter ADR and conformance |

### Amendment: neutral platform-provider realization contract (2026-07-26)

An accepted downstream platform adapter may advertise
`PlatformProviderAdapter` for Native inference. The publication still pins the
exact credential reference and an exact Platform trust-domain holder; dispatch
still atomically binds that tuple to the claim epoch. The downstream adapter may
then exchange it for a short-lived gateway capability while the Worker and
workload remain provider-secret-free, and records the ordinary claim-fenced
realization receipt. Awaken defines only this neutral execution fact: it does not
define a cloud gateway, lease protocol, IAM policy, provider route, or secret
store. Absence of the exact holder, capability evidence, ownership fence, or
receipt fails closed and never falls back to Worker or workload plaintext.

### Amendment: platform-held Connector effects (2026-08-13)

A hosted Connector uses `PlatformRelay` as the neutral realization fact for one
trusted platform egress process. That process composes the existing
`PinnedCredentialMaterializer` over the canonical `CredentialRepo` and
`SecretStore`, selects an exact opaque Platform trust-domain holder, and
resolves an immutable `CredentialAccess` only with the exact Workspace and
target/use fingerprint. `CredentialUsage::HttpEffect` binds each declared field
to exact header, query, or RFC 6901 JSON-pointer destinations. The Gateway must
prove that the effect's actual reference set equals this frozen usage before it
substitutes the material and performs the bounded effect in the same process.
The secret-free caller receives only the bounded upstream result and an ordinary
realization receipt; credential material never crosses that process boundary.

This amendment adds neither a Connector request DTO nor a material-transport
protocol. The product owns its effect DTO. Its downstream Gateway owns route,
lease, request-bound, forwarding, and response behavior. Awaken owns exact
credential admission and materialization. The Gateway opens the same canonical
store adapters; a copied Vault, local fallback, or raw-secret RPC is not an
equivalent realization.

`CredentialEnvelope` currently records recipient, expiry, and payload
fingerprint constraints for an installed resolver such as CSI. Awaken does not
currently implement a cryptographic envelope issuer, recipient public-key
registry, ciphertext store, replay nonce, or KMS unwrap contract. A deployment
that cannot install the canonical materializer in the Gateway therefore fails
closed; it must not claim recipient-bound network delivery from envelope
metadata alone.

### Amendment: deployment-owned Cloud Native profile (2026-08-14)

The canonical Environment execution application accepts one optional,
deployment-owned `CredentialRealizationProfile` for Cloud Native Sessions and
freezes it into the existing `EnvironmentSnapshot`. The hosted composition must
derive that profile from the same installed adapter capability projection used
by its Workers. Self-hosted Native and all ACP Sessions retain their canonical
local profiles. This seam does not enumerate allowed holders, select from model
policy, or retry with a different boundary; absence preserves the open
self-hosted default, while an installed profile selects exactly one holder per
purpose before Session persistence and dispatch.

An unsupported matrix cell fails admission with a stable error. It never falls
through to another row.

### Amendment: Repository Gateway mediation (2026-08-21)

Repository, Model, MCP, and Connector credentials continue to share the same
`CredentialAccess` admission kernel and the same deployment-selected plaintext
holder. They do not acquire a second credential hierarchy or a generic secret
transport DTO. The target adapter remains responsible for its final injection:
provider headers for Model, the selected delivery channel for MCP, exact effect
placements for Connector, and HTTP Basic transport for Repository.

For a Worker-held Repository credential, the existing direct
`CredentialMaterialResolver` and `WorkerRelay` path remains the self-hosted
implementation. For a Platform-held Repository credential, the existing
claim-fenced Repository verification boundary returns a short-lived mediated
Git endpoint and capability. The Worker gives that capability to its unchanged
Repository realizer as ephemeral HTTP Basic transport; the Gateway validates
the exact Session, Run claim, Workspace, Repository config, and credential
revision, opens the canonical credential store, and substitutes the upstream
material in-process. Coordinator staging without a Run claim remains
secret-free and performs no materialization.

The mediated capability is bounded by the exact dispatch claim expiry read by
the existing commit-epoch guard. It may be shorter than the configured Gateway
TTL, but it must never remain valid after the claim that authorized it.

The selected holder is authoritative. A Platform-held Repository cannot fall
back to Worker plaintext when the Gateway, authorization, credential store, or
upstream is unavailable. Conversely, an open self-hosted composition that does
not install the deployment authorizer retains direct Worker injection. This is
one holder-selected realization path, not runtime discovery or fallback.

The stable admission failures are:

```rust
enum CredentialAdmissionError {
    EmptyAllowedHolders,
    HolderNotAllowed,
    HolderUnsupported,
    MaterialSourceUnsupported,
    EnvelopeRecipientMismatch,
    EnvelopeExpired,
    DirectPublicationRejected,
    CredentialRevisionMismatch,
}
```

Adapters may add private diagnostics, but public boundaries map them to these
source-domain failures without exposing material or trying another holder.

### D8: automatic LLM Vault authoring is a separate proposed workflow

This ADR does not make automatic LLM Vault authoring implementation-ready. A
future proposal must name at least:

- one `ModelCredentialAuthoringService` owner in the configuration application
  layer;
- the Credential, Model binding, publication, idempotency, and outbox repository
  ports it coordinates;
- whether one database unit of work is possible;
- the durable saga/operation record when `SecretStore` or repositories are not
  transactionally co-located;
- compensation and orphan-material reclamation;
- one-time secret input, redacted responses, rotation, and retry behavior.

Until that proposal is accepted, authoring continues through the existing
explicit Credential and Model publication paths. No environment discovery or
Session request may silently persist or bind an LLM credential.

## Static Structure

```text
Credential/Vault context
CredentialSource + SecretStore
        │ exact id/revision
        ▼
CredentialAccess
├── material source + optional recipient-bound sealed payload reference
├── existing CredentialUsage
├── optional exact OAuth refresh/reseal access
└── CredentialExecutionPolicy
    ├── allowed PlaintextHolder set
    └── Forbidden | VirtualOnly
        │
        ├─────────────────────────────┐
        ▼                             ▼
ResolvedModelCandidate          SessionMcpAttachment
(sole Model authority)          (generation authority)
        │                             │
        ▼                             │
AttemptCredentialBinding              │
        │                             │
        ▼                             ▼
CredentialMaterialResolver      CredentialMaterialResolver
        │                             │
        ▼                             ▼
Model materializer              Runtime Host MCP realization
        │                             │
        └── secret-free realization receipt / exact holder ───┐
                                                               ▼
                                                   Sandbox/Worker/platform
                                                   implementation + receipt
```

`ResolvedRepositoryCredential` is a third consumer of the same exact access,
holder-admission, and material-resolution contracts. It remains nested in the
resolved Repository input owned by the Resource/Session lifecycle; it does not
join `SessionMcpAttachmentSet` and does not create a common Service authority.

## Dynamic Behavior

### Model publication and execution

1. Configuration publication resolves one exact credential id/revision, material
   source, optional recipient-bound sealed payload reference, usage, optional
   exact refresh access, and execution policy into
   `ResolvedModelCandidate`.
2. Dispatch copies the immutable candidate; it never reselects credentials.
3. The exact Environment/execution profile requests one holder. Admission proves
   that exact holder belongs to the allowed set and is supported by the installed
   materializer/provider.
4. Durable dispatch atomically commits `AttemptCredentialBinding` with the claim
   owner, Worker incarnation, lease, and monotonic epoch. An explicitly
   non-durable direct Run compiles the same binding before provider I/O and
   fences it to the exact active Session `run_id`; it is intentionally
   unrecoverable. An unsupported holder fails admission in either topology.
5. The exact material resolver and materializer validate the candidate, binding,
   envelope recipient/expiry, and claim epoch and
   realizes Native or ACP access at the selected boundary.
6. A successful effect returns a secret-free receipt with the actual mechanism.
   Retry/recovery under that epoch reuses the binding. Revocation,
   owner/revision mismatch, or failed materialization fails closed; another
   holder is not selected as fallback.

### MCP attachment realization

1. Managed authorization and normalization freeze one exact `CredentialAccess`.
2. The frozen Environment/execution profile requests one exact holder; Session
   admission validates and persists it with the new MCP generation before
   network I/O.
3. Host checks ownership and invokes the corresponding Worker/process adapter.
4. The exact material resolver supplies the selected boundary and reconstructs
   optional OAuth refresh/reseal behavior only from the pinned access. A
   successful generation CAS makes the route visible; its receipt records no
   plaintext.
5. Remove, replacement, ownership loss, or lease expiry first hides and rejects
   new use, then drains and clears material.
6. Recovery recreates realization from exact references and policy; it never
   restores serialized plaintext or selects a different holder.

### 2026-08-14 amendment: authentication refresh is effect-aware

Receiving an authentication challenge authorizes refresh of the exact pinned
credential; it does not by itself authorize replay of an arbitrary MCP request.
The canonical MCP wire layer classifies protocol methods for this decision.
Initialization, discovery and other explicitly read-only methods may be retried
once after a successful refresh. `tools/call`, unknown methods and malformed
messages are never automatically resent by the relay. Their response or
transport failure is returned to the owning executor, whose existing recovery
boundary decides the terminal outcome.

The relay remains an opaque generation-fenced credential mediator. It does not
parse tool arguments, persist an effect journal, promise exactly-once execution,
or create a second Tool authority. In particular, an ACP Worker loss after an
MCP request may have been dispatched terminates the recovered opaque ACP execution as
`EndCause::Indeterminate` instead of replaying its prompt.

### Repository Resource realization

1. The Resource Catalog persists one immutable Repository config version with a
   Vault source binding, never material or a Runtime-ready credential decision.
2. Session creation or hot Resource replacement resolves that binding exactly
   once against the trusted Workspace and active source revision. It persists a
   `ResolvedRepositoryCredential` with canonical HTTP usage, `Forbidden` model
   exposure, and the baseline's exact `resource_holder` before activation.
3. Runtime rejects a missing pin for a bound Repository, a pin for an anonymous
   Repository, source/usage/holder mismatch, or stale source revision before a
   Git side effect. It admits `WorkerRelay` and calls the common exact material
   resolver; there is no Runtime API that opens a bare Vault source id.
4. The Worker translates the admitted typed document once into the
   non-serializable `RepositoryHttpBasicCredential` accepted by the Repository
   realizer. The material is used only by the host-mediated Git transport.
   The persisted Session manifest, prompts, events, and sandbox-origin metadata
   remain secret-free. Replacing the Repository binding creates and commits a
   new config version and exact pin; it never mutates the old pin in place.
5. A retained pre-pin Session row crosses the same compiler once under the root
   Session CAS before recovery I/O. An already pinned row is unchanged; a
   missing compiler or no-longer-active/in-Workspace source fails closed. Runtime
   never regains the deleted bare-source path as a compatibility fallback.

## Failure, Retry, and Terminal Rules

- unsupported holder, missing provider capability, unavailable broker, or
  out-of-policy network target fails admission;
- no failure may switch from Worker/Platform mediation to workload plaintext;
- idempotent retry uses the same credential revision, holder, target, and
  attachment generation;
- a stale generation or ownership epoch cannot perform an external effect;
- cleanup failure remains retryable after visibility and authority are revoked;
- logs, errors, events, receipts, snapshots, queues, and Session rows contain no
  plaintext or virtual capability value.

## Ownership and Dependency Rules

| Owner | Owns | Must not own |
|---|---|---|
| Credential/Vault | credential identity, revision, status, material references/storage, exact OAuth refresh/reseal configuration | Model/MCP target or Session generation |
| Config publication/runtime contract | secret-free `CredentialAccess`, material source/envelope reference, usage, refresh access, policy in the executable snapshot | plaintext, IAM decision, realization route |
| Model context | exact `ResolvedModelCandidate` and fallback order | Session MCP state or runtime Vault search |
| Dispatch claim transaction | exact attempt-epoch candidate fingerprint, selected holder, planned realization, Worker/lease fence | model/credential selection or fallback after failure |
| Session context | MCP generation and selected allowed holder; resolved Repository input with exact access/holder pin | credential enumeration, material opening, or model selection |
| Credential material resolver | exact source/envelope opening and recipient/revision/use validation | enumeration, holder selection, target selection, or cross-boundary material return |
| Runtime Host/materializer | exact validation, realization, OAuth refresh/reseal execution, ownership checks, receipt | new credential/holder selection after failure |
| Provisioning provider | last-mile process/file and network enforcement | policy authorship, route selection, Vault schema |
| Downstream platform adapter | its opaque trust-domain lease and gateway behavior | reverse dependency or Awaken domain changes |
| Runtime Core | consume the installed executor/capability projection | Vault, holder selection, Sandbox, gateway |

## Required Consolidation

1. amend `CredentialAccess` rather than introducing `CredentialDelivery`;
2. split `CredentialInjectionKind` into material-source and optional
   recipient-bound envelope-reference semantics and install one exact material
   resolver port;
3. reject new `Direct` publication and bound any compatibility decoding;
4. keep `ResolvedModelCandidate` as the sole inference access authority;
5. remove inference Service projection from ADRs and design catalogs;
6. persist exact holder and planned realization selection atomically with the
   dispatch claim owner/epoch/Worker lease in `AttemptCredentialBinding`, and
   persist the holder in MCP attachment generations;
7. return actual mechanism only in a secret-free realization receipt and reject
   stale-epoch effects;
8. compile existing MCP OAuth refresh/reseal configuration into exact
   `CredentialRefreshAccess`; delete URL/current-Vault rediscovery without
   deleting refresh behavior;
9. replace ACP `api_key: String`/ordinary inline env with an exact typed
   last-mile requirement before claiming Workload plaintext conformance;
10. require substitution plus no-bypass networking before claiming Worker-held
    MCP behavior;
11. replace bare Repository source materialization with the Session-persisted
    exact access/holder pin and the common exact material resolver;
12. design automatic LLM Vault authoring separately before implementation.

## Verification

| Claim | Required evidence |
|---|---|
| access is one authority | publication/fingerprint tests and no runtime credential enumeration |
| sealed transport is complete | envelope reference/payload fingerprint, exact recipient, expiry, replay, and resolver conformance |
| explicit holder authorization | allowed-set selection, trust-domain mismatch, empty-set rejection |
| deterministic holder selection | exact Environment/profile request; ambiguous capability sets never influence the result |
| `Forbidden` exposure | prompt/tool/resource/model payload scans contain no real or virtual credential |
| `VirtualOnly` exposure | only scoped synthetic values appear; expiry/target/generation/revocation enforced |
| Workload plaintext | process-only env/private-file permissions, cleanup, no base-env persistence |
| Worker plaintext | real secret absent from Sandbox, substitution works, network bypass blocked, route revoked |
| platform plaintext | Worker cannot materialize backing secret; opaque trust-domain lease only |
| no downgrade | capability, network, relay, gateway, reconnect, and lease failures all fail closed |
| Native/ACP parity | same candidate, credential revision, allowed holder policy, and selected holder |
| attempt durability | claim transaction atomically stores binding/epoch; retry reuses it; reclaim mints a new fenced binding |
| OAuth continuity | exact refresh config, public/confidential client exchange, access/refresh-token reseal, restart, and no URL rediscovery |
| Repository continuity | config binding compiles to one exact revision/holder/usage pin; anonymous, stale, inactive, cross-Workspace, mismatched, and missing-pin cases fail before Git I/O; no bare-source Runtime materialization remains |
| secret-free state | serialization/database/event/log/error/receipt/queue fixture scans |

TypeScript E2E belongs to the slice that exposes each behavior. The first MCP
slice proves create/call and secret absence. Later slices add add/replace/remove,
restart, ownership-loss, and mediated/no-bypass fixtures. Automatic LLM Vault E2E
is deferred with its authoring proposal.

## Consequences

- Trust domains are explicitly authorized rather than inferred from a false
  strength order.
- Model exposure remains orthogonal to which process holds real material.
- Existing credential access/usage vocabulary is extended instead of duplicated.
- Model and MCP reuse security values without merging their aggregates.
- Vault remains storage at rest, not an execution custody grade.
- Automatic authoring and unimplemented Platform cells cannot be claimed before
  their application and trust boundaries are designed and tested. Native
  Platform inference is limited to the neutral amendment's exact adapter contract.

## Rejected Alternatives

- **Ordered custody floor.** Worker and platform trust domains are not naturally
  substitutable.
- **`PlaintextAllowed` model exposure now.** There is no approved driving case.
- **A second `CredentialDelivery` policy enum.** Existing access, material
  source/envelope, usage, and provider primitives already own those facts.
- **Persist inference as a Session Service attachment.** It duplicates
  `ResolvedModelCandidate`.
- **Use Session `vault_ids` for model credentials.** It introduces a second model
  credential authority.
- **Call `EgressOnly` a custody proof.** Substitution without no-bypass networking
  still exposes the original target path.
- **Include automatic LLM Vault authoring in this execution ADR.** Its transaction
  and compensation lifecycle belongs to a separate application workflow.

## Development Readiness and Implementation Gate

The coordinating ADR-0066 Slice 0 and the in-repository Workload/Worker contract
paths have landed: envelope-reference and material-resolver values, atomic
claim-epoch binding and recovery shape, exact OAuth refresh/reseal projection,
typed ACP last-mile requirements, Repository pins, and the neutral Native
`PlatformProviderAdapter` extension are current code. Their decision-table,
secret-nonleak, retry/reclaim, and Rust/TypeScript integration tests are the
regression gate for further development.

The explicit holder policy, no-order/no-fallback rule, Model/MCP aggregate
separation, `Forbidden`/`VirtualOnly` exposure, and the Native/ACP/MCP matrix are
accepted. `Direct` remains decode-only for legacy snapshots and is rejected by
every new publication; the compatibility decoder is removed after all stored
snapshot schema versions that can contain it are migrated or expire under
retention policy.

G43 remains a target guardrail for the deployment-specific authenticated ACP MCP
case that must prove Worker-held substitution and provider-enforced no-bypass
networking; built-in providers fail closed when they cannot prove that cell.
Platform ACP/MCP realization and automatic LLM Vault authoring remain separately
gated work and cannot be inferred from the implemented Native platform contract.

## 2026-08-15 amendment: Session MCP admission joins the existing claim boundary

The frozen Session runtime projection is now a public run-ingress contract value
rather than a private Runtime Host serialization shape. Before committing a
Worker claim, the existing dispatch credential compiler decodes that projection
and admits every MCP generation's paired access and selected Worker holder
against the Worker's installed materializer capabilities through the same
`CredentialAccess::admit` kernel used by inference. Broad selection skips a row
that cannot admit the complete frozen credential set; an exact claim returns the
typed run-ingress admission failure before any external effect.

The Session MCP generation remains the sole durable holder and effect-receipt
authority. Dispatch persists no parallel MCP attempt binding, Runtime performs
no credential or holder reselection, and the generic runtime credential contract
contains no Session vocabulary. Material-source or refresh-provider
unavailability relinquishes the claim through the existing retry path; invalid
projection, policy, or holder facts remain absorbing failures.

## 2026-08-15 amendment: hosted sealing extends the exact Vault boundary

The canonical Vault compiler may receive one deployment-owned
`CredentialEnvelopeIssuer` port. It first validates the active source, exact
Workspace, revision, selected holder, usage and target binding through the
existing Credential repository and policy. Only then may the issuer receive
that exact material and return the existing recipient-bound
`CredentialEnvelope`; the secret-free `CredentialAccess` remains the sole
published execution value.

The open self-hosted composition installs no issuer and retains its existing
in-process materialization. A hosted deployment may implement cryptographic
sealing and Worker-private unsealing behind this port, but it may not add a
plaintext material API, copied Vault, independent credential selector, or a
second access contract. Issuance failure fails the compilation operation; it
never falls back to an unsealed hosted reference or a different holder.

## 2026-08-26 amendment: source descriptors bind exact target and usage

`CredentialSource` remains the sole secret-free source row. A newly described
source stores one optional `CredentialDescriptor` on that row; legacy rows omit
it. The descriptor owns the provider, material kind/type and complete structured
field set, optional subject/permissions/static expiry, and a list of exact
target contracts. Every declaration is one indivisible target identity
`(purpose, audience)` plus `CredentialUsage`. Separate purpose/audience sets and purpose-to-usage inference are
forbidden: they would respectively authorize an undeclared Cartesian product or
create a second usage authority. Executable `CredentialAccess` carries the
target identity and its existing `usage` field once; it does not serialize a
second usage inside the target.

For a described source, access compilation requires one exact declared target
and freezes it in `CredentialAccess`; the target's declared usage must equal the
access usage. The materializer repeats that comparison at the plaintext edge,
checks static expiry, opens only the pinned active revision, then proves the
opened material kind/type/complete field set equals the same descriptor. The
canonical Git transport remains `awaken.http-basic/v1` with exactly `username`
and `password`; a Connector API credential remains a scalar secret under its
exact HTTP-effect target. Core does not introduce a dual-use HTTP token shape or
reinterpret a structured field as an ordinary header secret.

The existing admin create, exact read, expected-version rotate, and
expected-version retirement routes carry the optional descriptor or revision
fence. They reuse the existing Credential WAL, repository, SecretStore, and
source revision CAS. Create or material replacement validates
descriptor/material equality and proves the opened material supports every
declared target usage before any storage effect. A descriptor-only
rotation may change subject, permissions, expiry, or targets only while
retaining the material descriptor; changing material shape requires replacement
material in that same CAS. `provider_id` remains legacy compatibility metadata
only for rows without a descriptor. New described writes reject that field, and
a deserialized row containing both provider representations is invalid; there is
never a dual-provider source requiring equality checks or synchronization. Once
a descriptor is published, its provider is immutable because it participates in
source/idempotency identity; rotation may change subject, permissions, expiry,
targets, and compatible material, but never reassign the source to a provider.

Repository Session compilation derives its exact target audience through the
shared HTTPS-origin normalizer (`https://<normalized-origin>/git`) plus canonical
HTTP-Basic usage, includes the repository id/version separately in the existing
material binding, and passes both through the existing Control credential-access
port. Repository paths on one origin can reuse a credential; another origin,
userinfo, or a non-HTTPS transport fails closed. Newly entered Repository
credentials are always described. An undescribed legacy source cannot be
rebound to a caller-selected Repository or HTTP-effect target; it requires an
explicit migration and remains executable only through the already targetless
Provider/MCP compatibility paths. No issuer, live-probe contract, GitHub App
adapter, credential catalog, or second store is introduced by this amendment.
Provider-managed acquisition and per-issuance expiry remain deferred until a
production caller and adapter can close that port; a short-lived issued expiry
must not be frozen into static source metadata.

The descriptor support matrix is closed. `RepositoryTransport` admits only
`HttpBasicAuth`, `HttpEffect` admits only the existing typed HTTP-effect usage,
`SignatureVerification` admits only an exact material-field to
provider-neutral-algorithm map, and `Extension` admits only the existing
extension usage (whose `consumer_id` remains the sole extension-consumer
identity). Described `ProviderAdapter` and
`McpAuthorization` sources are rejected until their existing publication
compilers carry an exact target; legacy undescribed Provider and MCP sources
remain unchanged. Repository, HTTP-effect, signature-verification, and extension
access therefore requires a target. The same target/usage rule runs at
descriptor validation, access admission, and before either local or external
material resolution.

The Credential Vault owns one pure source-to-access compiler for active status,
Workspace, positive revision, expiry, holder policy, binding, descriptor
admission, and target attachment. Exact-target consumers supply the already
selected holder. The retained undescribed, targetless Provider and A2A
publication paths instead supply one typed deferred-selection owner because the
dispatch claim selects their exact Worker or Workload holder; deferral is
rejected for every described/target-bearing source, empty holder policy, or
usage owned by another consumer. Protocol and hosted adapters may add custody
delivery only after that compiler succeeds; they do not reimplement source-row
admission. Runtime
Repository activation retains only the secret-free exact pin. Direct clone,
hot replacement, and terminal publish each reopen that same pinned revision at
the Git effect edge and drop HTTP Basic material when the operation returns.
Rotation, revocation, expiry, target drift, or Workspace drift therefore stops
the next Git effect before I/O; Gateway-mediated access refreshes only its exact
capability and never changes to Direct.

Every material rotation and terminal retirement consumes an expected source
revision and enters the same Vault WAL/CAS. There is no read-latest mutation
helper: a stale Session, write-back, refresh, archive, or admin command
conflicts before material is written or erased. Generic Secret mounts have no
target/usage wire, so they reject every
described source for both read and write-back; they remain an explicitly
targetless legacy compatibility surface rather than a bypass around the exact
access compiler. Likewise, Provider and A2A selection excludes described
sources until those existing consumers own canonical target compilers.
