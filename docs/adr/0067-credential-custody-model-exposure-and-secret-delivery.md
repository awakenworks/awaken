# ADR-0067: Credential Plaintext Boundaries and Model Exposure

- Status: Accepted
- Date: 2026-07-24
- Accepted: 2026-07-25
- Builds on: [ADR-0062](0062-published-inference-access-and-runtime-credential-injection.md)
  (one exact published model candidate and credential access pin)
- Coordinates with:
  [ADR-0066](0066-session-service-binding-and-realization.md)
  (Session baseline and MCP attachment lifecycle)
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
requested holder is a new Session MCP generation or Run attempt, never a failure
fallback.

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

trait CredentialMaterialResolver {
    async fn resolve_exact(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
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

`CredentialUsage` remains authoritative for `ProviderAdapter`, HTTP header/query,
client certificate, environment variable, and file semantics. MCP, Model, and
Resource adapters must not invent protocol-specific credential-usage fields.

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

`CredentialMaterialResolver` is the sole neutral material-source port. A Control
reference adapter, Worker-private adapter, or recipient-bound envelope adapter
may implement it, but all consume the same exact access and holder selection.
The port cannot enumerate credentials, choose another revision or holder, or
return material to a boundary different from the selected holder. Hosted Vault,
IAM, gateway, and transport types remain outside the contract.

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

The selected plaintext holder is an execution fact owned by one durable dispatch
claim epoch:

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

Retries and response-loss recovery under the same claim epoch reuse the binding
exactly. Reclaim advances the epoch and creates a new attempt-epoch binding only
after the newly selected Worker and frozen execution profile request one exact
allowed holder. Failure within an epoch never changes it. A stale epoch cannot
materialize or commit a receipt. Native and ACP projections for one attempt
consume the same binding.

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

`EnvVisibility::EgressOnly` is only a delivery hint. It proves nothing unless the
selected provider supports substitution and the network policy prevents bypass.

### D7: provisioning executes selected last-mile requirements only

`awaken-provisioning-contract` continues to expose neutral process/Sandbox
primitives. A Sandbox provider may execute an already-selected process-secret or
secret-file requirement through `EnvValue::Secret`, `MountSource::Secret`, and
`SecretBroker`.

It must not enumerate credentials, select a revision, authorize a trust domain,
select an MCP target, or own relay/gateway lifecycle. Network realization remains
`NetworkPolicy` enforcement, not credential policy authorship.

The initial implementation matrix is closed:

| Selected holder | Native inference | ACP | MCP | Required evidence |
|---|---|---|---|---|
| Workload | not used for in-process Native provider access | typed process-secret or private secret-file requirement | only an explicitly trusted workload MCP client | last-mile delivery, process/file scope, cleanup, no base-env persistence |
| Worker | exact Worker provider adapter | Worker-mediated endpoint; no real secret in ACP workload | generation-fenced Worker MCP relay | exact substitution, no-bypass network, ownership/lease revoke |
| Platform | deferred | deferred | deferred | separate accepted downstream-adapter ADR and conformance |

An unsupported matrix cell fails admission with a stable error. It never falls
through to another row.

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
4. The dispatch claim transaction commits `AttemptCredentialBinding` with the
   claim owner, Worker incarnation, lease, and monotonic epoch before provider
   or ACP materialization. An unsupported holder fails the claim/admission.
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
| Session context | MCP generation and selected allowed holder | credential enumeration or model selection |
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
11. design automatic LLM Vault authoring separately before implementation.

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
- Automatic authoring and platform mediation cannot be claimed before their
  application and trust boundaries are designed and tested.

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

This ADR is an accepted target architecture, but Workload and Worker realization
are implementation-gated by the coordinating Slice 0 in ADR-0066. Before feature
coding begins, the runtime contract must freeze the envelope-reference schema and
material-resolver result, dispatch must freeze the atomic claim-epoch binding
transaction and recovery wire shape, Credential/Vault must freeze exact OAuth
refresh/reseal projection, and ACP must freeze its typed last-mile requirement.
Those are contract decisions, not private implementation details.

The explicit holder policy, no-order/no-fallback rule, Model/MCP aggregate
separation, `Forbidden`/`VirtualOnly` exposure, and the Native/ACP/MCP matrix are
accepted. `Direct` remains decode-only for legacy snapshots and is rejected by
every new publication; the compatibility decoder is removed after all stored
snapshot schema versions that can contain it are migrated or expire under
retention policy.

Acceptance approves the target design, not current conformance. G43 remains a
target guardrail until one Workload and one Worker realization have the required
Rust/provider/network and TypeScript end-to-end evidence. Platform realization
and automatic LLM Vault authoring remain separately gated work and cannot be
inferred from this acceptance.
