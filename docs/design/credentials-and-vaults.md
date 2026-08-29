# Credentials, Vaults, And Availability

> The complete consolidated model — agent config + model catalog + provider +
> vault/credential + the runtime-unaware secret seam + security layers + staging —
> is decided in [ADR-0043](../adr/0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md).
> This page states the boundary principle; the ADR carries the model.

Credential lifecycle is a credential-domain/product concern. Runtime code should see
only opaque references and explicit permission decisions.

## Bounded Context Split

| Context | Owns |
|---|---|
| Runtime Core | typed tool calls, permission hooks, opaque credential references |
| Dispatch / Server | passing resolved references into runtime activation when required |
| Credential Domain / Product | vault schema, credential CRUD, refresh policy, account grouping, availability projection, operator UX |
| Credential materialization adapter | exact pinned material lookup and OAuth refresh/reseal mechanics; no selection or owning API |
| Orchestration layer above | credential delivery into tool execution (out of scope here) |

## Domain Model

| DDD type | Name | Rule |
|---|---|---|
| Entity | `CredentialRecord` | Stores identity and metadata, not public grants |
| Entity | `Account` | Groups credentials for one provider principal |
| Value object | `CredentialRef` | Opaque outside the credential context |
| Domain service | credential selector | Chooses an eligible candidate, never authorizes |
| Domain service | refresh coordinator | Single-flight refresh and atomic write-back |
| Projection | availability state | Operational status, not access control |

## Runtime Boundary

The runtime receives an **already-resolved** credential value (`RedactedString`) or
nothing — never a ref, handle, or resolver (D6/D9; the host resolves upstream, see
[ADR-0043](../adr/0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)).
This supersedes the earlier "opaque `CredentialRef` in runtime inputs" framing:
the resolution seam moved out of the runtime into the host. Runtime must not know:

- vault table layout;
- OAuth grant/refresh schema;
- tenant sharing rules;
- provider billing quota;
- public credential API shape.

Tool execution asks the permission path whether use is allowed. The credential
context supplies material only after authorization.

## Selection Is Not Authorization

Selection answers "which candidate can be used if policy allows it?" It never
answers "is the caller allowed?"

Availability checks follow the same rule:

- success may clear a cooldown/login-required projection;
- failure or unknown never marks a credential available;
- probe result carries no grant;
- disabling/enabling is an operator action in the product context.

## First Vertical Slice

For product-owned credential work, build slices in this order:

1. secret-in/secret-free-out credential CRUD;
2. opaque `CredentialRef` resolution for one provider;
3. explicit permission check before materialization;
4. single-flight refresh if the provider uses rotating credentials;
5. availability projection and manual check endpoint.

Do not implement credential pools, account spreading, or quota routing until the
single-provider flow is working.

## Non-Goals

- No vault schemas in runtime crates.
- No grant field on selector, probe, or capability results.
- No ambient environment fallback for missing credential refs.
- No product policy in store/repository implementations.

## Concrete Model

This model retains the useful oversight-next separation while following Awaken's
current authority graph. Two **orthogonal** axes must not be conflated:

- **Materialization** — *where* a secret physically lives: `CredentialSource.kind`.
- **Selection** — *which* source a run uses: `CredentialBinding` (+ `CredentialPool`).

There is **no inline or ambient-secret-in-config path**: an executable secret
always lives in a persisted `CredentialSource` (vault or an explicitly persisted
OAuth helper), never embedded in a spec or read from provider process environment.
Environment inspection may produce a secret-free UI proposal only. The runtime sees neither axis — both resolve management-side into a
`SecretInput` (see [ADR-0043](../adr/0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)).

### Credential source — the stored row (secret-free)

```rust
struct CredentialSource {                     // supersedes this page's earlier `CredentialRecord`
    id, workspace_id,
    kind: CredentialKind,                     // Vault | Oauth (`Env` decodes legacy rows but cannot execute)
    descriptor: Option<CredentialDescriptor>, // canonical provider + material + exact target/usage declarations
    provider_id: Option<String>,               // legacy-only; forbidden when descriptor is present
    protocol_endpoint_id: Option<String>,     // optional exact endpoint proof/scope
    adapter_registration_id: Option<String>,
    auth: CredentialAuth,                     // neutral; ACL maps the managed wire tags
    material_ref: Option<SecretRef>,          // → SecretStore; None for OAuth helpers
    status, max_concurrency,
    account_id: Option<CredentialAccountId>,  // shared upstream-account quota bucket
    version,
}
enum CredentialAuth {                         // one Vault holds BOTH LLM and MCP credentials
    ApiKey { env_key: Option<String> },       // LLM inference key
    Bearer { mcp_server_url },                 // MCP static bearer
    OAuth  { /* refresh material */ },         // MCP OAuth (auto-refresh)
    EnvVar { secret_name, networking },        // generic egress secret
}
enum Networking { Unrestricted, Limited { allowed_hosts: Vec<String> } }
```

### Selection — binding + pool

```rust
enum CredentialBinding {                       // the "which credential" axis (oversight-next)
    None,
    InheritDefault,                            // inherit from the owning execution context
    Exact { credential_source_id },            // one source
    OneOfCredentialPool { credential_pool_id }, // choose one pool member
}
struct CredentialPool {                        // a set of interchangeable sources
    id, workspace_id, slug, display_name,
    provider_id: Option<String>, adapter_kind: Option<String>,  // optional homogeneity
    selection_policy: CredentialSelectionPolicy, status, version,
}
struct CredentialPoolMember { pool_id, credential_source_id, ordinal, enabled, selection_weight, health_override }
enum CredentialSelectionPolicy { OrderedFallback, LeastRecentlyUsed, Priority, CostWeighted }
```

### Provider access and routing policy

```rust
struct ProfileCandidate {
    target: ModelTarget,                        // provider + optional exact endpoint + model
    credential_binding: CredentialBinding,      // vault-backed, never inline
}
struct InferenceProfile {
    workspace_id, primary: ProfileCandidate, fallbacks: Vec<ProfileCandidate>,
    disabled_endpoint_ids: Vec<ProtocolEndpointId>,
}
```

There is no second persisted `ProviderIdentity` aggregate. `ProfileCandidate`
owns the explicit target×credential selection and `InferenceProfile` owns
operator routing/fallback and endpoint-disable policy. A
`CredentialSource.protocol_endpoint_id` is narrower and immutable: a Provider
connection records the exact endpoint on which that credential was proved. It
may restrict selection but never enables or disables a route. A manually entered
provider-wide source leaves it absent. This separates credential proof scope
from routing policy without synchronizing two endpoint lists.

The vault container and the public Managed wire are unchanged:

```rust
struct Vault { id, workspace_id, display_name, metadata }   // container aggregate
```

`RedactedString` (`secrecy::SecretBox` + zeroize + redacted `Debug`/`Display`; the
sole plaintext accessor is `expose_secret()`) is the one secret value object.

The routing entities that combine credential × model × protocol
(`ProtocolEndpoint`, `Offering`, `InferenceProfile`, `InferenceTriple`,
`resolve_inference`) live in a dedicated management-plane crate
`awaken-management-contract` (ADR-0088) — see
[model-provider-backend-binding](model-provider-backend-binding.md).

## Front-Door Wire Alignment (Managed Agents)

The `awaken-protocol-managed` front door MUST mirror the **official Anthropic
`@anthropic-ai/sdk` Managed Agents** vault/credential resource — not a
second-hand port. The authoritative shapes are the SDK's `beta.vaults.*` /
`beta.vaults.credentials.*` types (`BetaManagedAgentsVault`,
`BetaManagedAgentsCredential`, and the `*AuthResponse` / `*CreateParams`
variants); awaken-next's `managed-contract` is itself a mirror of these, so treat
it as a cross-check, never the source. Never guess a shape (ADR-0037/G16).

Wire facts to match exactly:

- Objects `vault` / `credential` (the `type` discriminator field), snake_case.
- Credential `auth` union tagged by `type`: `mcp_oauth`, `static_bearer`,
  `environment_variable` (+ the OAuth `refresh` / `token_endpoint_auth` shapes).
- `environment_variable` carries `networking` (`unrestricted` | `limited{allowed_hosts}`)
  and `injection_location` (`{header, body}`).
- Validation status `valid` / `invalid` / `unknown`; MCP-OAuth validate endpoint
  (`credentials/{id}/mcp_oauth_validate`).
- Constraints: unique key per vault (`mcp_server_url` / `secret_name`), keys
  immutable, secret fields write-only, max 20 credentials per vault.
- Beta header `managed-agents-2026-04-01`; sessions attach vaults via `vault_ids`.

Every Managed credential create/update/archive/delete is one Credential
bounded-context command, not an HTTP-layer dual write. A secret-free
`PendingManagedCredentialMutation` freezes the exact Source and child before/
after pair around material IO. It and the source-only
`CredentialMutationIntent` flatten the same `CredentialMaterialMutationFence`;
their two existing JSON tables remain separate only because one publishes a
Source and the other publishes the wider Source+child pair. Create begins in
`Writing`; update begins there only when it introduces new material.
Metadata-only update, archive, and delete introduce no material and begin in
`Ready`. New-command admission accepts only the constructor's canonical fresh
shape: material-writing work is original-owner epoch 1 with token equal to its
attempt id and a live lease, while material-free work has no owner/attempt and
is already `Ready`. Serde fields cannot submit a forged `Ready` new-material
command.

`begin` returns physical-attempt ownership. An exact or logically identical
fresh retry finds the source-keyed durable pending row, reports non-ownership,
and returns a transient pending conflict before any SecretStore write; it never
creates a second WAL/ref or writes through the durable owner's token. Plaintext
is intentionally absent from this comparison, so a same-projection retry with
different bytes receives the same transient result while the winner is pending.
After publication, a source-only idempotent command's stable identity compares
logical metadata/material slots, reads the
actual durable refs, and verifies exact bytes, so retry converges without a
second WAL or physical ref.

Only the exact original writer may advance `Writing` to `Ready`, after every
material put has returned. Recovery may repository-CAS claim an expired
`Writing` fact, but the claimed epoch can only advance to `ReclaimingAbort`.
Recovery never inspects those bytes and promotes them: SecretStore has no
conditional-put CAS against the writer epoch, so a late old writer could still
overwrite that attempt ref. Only a durable `Ready` fact is eligible to publish.
The lease test uses injected process time and has no renewal heartbeat, so a
slow external write may be fenced; attempt-specific refs ensure its late write
can create only an unreachable orphan.

One `ManagedCredentialRepository` transaction locks the parent Vault, checks
both revision fences, publishes `CredentialSource` plus
`ManagedVaultCredential`, and advances the fact to `Reclaiming`. A non-create
commit writes its exact-revision rollout outbox event in that same transaction;
create has no rollout event. Periodic reconciliation publishes `Ready`, aborts
expired `Writing`, and completes `Reclaiming`/`ReclaimingAbort` cleanup work.

The child lifecycle is the closed `Active | Archived | Deleted` value;
`Deleted` is absorbing and physical purge is separate GC. The configured
rollout target adopts the event through the existing Session generation
protocol. AllInOne invokes one local Managed Session application; split Control
calls one configured authenticated private Coordinator endpoint with a
rotation-aware service token. One delivery pass queries that target's current
referencing Sessions. A non-idle Session or failed update keeps the event
pending; update prepares/publishes/drains the next MCP generation, while
archive/delete drain affected authenticated attachments and never downgrade
them to anonymous. Control deletes the row only after that configured target
accepts the complete event and the repository still contains the exact event
payload for its id. This is not a durable fleet-wide consumer registry or a
proof that no referencing Session appeared concurrently with the query.

The Managed Session repository owns discovery of those current references. Its
additive V2 schema stores a derived `(session_id, credential_source_id)` index
for the actual credential sources in the canonical desired MCP attachments; the
older Vault-membership reference retains its distinct meaning. Every Session
root create/update synchronizes the derived rows in the same root transaction,
and Session deletion removes them by the existing root relationship. SQLite and
Postgres constructors, including pre-provisioned-schema constructors, apply or
verify the migration and then transactionally clear and rebuild the complete
derived table by decoding canonical persisted roots before they return a serving
repository. A decode or transaction failure fails startup; restart repeats the
deterministic rebuild, so no readiness registry or SQL-side duplicate JSON model
exists. Vault rollout queries by exact Workspace plus source id.

Session adoption preserves that typed ownership. Public MCP desired-set
authoring enters the Session application as `SessionMcpUpdate::PublicReplacement`;
the Vault rollout enters as the distinct
`SessionMcpUpdate::CredentialLifecycle`. The latter may only replay or
monotonically advance the same source revision, or remove the exact attachments
affected by archive/delete. It cannot add or retarget MCP topology, alter an
unrelated credential, or carry Session title/metadata/budget/tool updates.
Consequently a Frozen or FileResources baseline can reject public re-authoring
without blocking credential rotation or revocation. The Session policy never
becomes another credential lifecycle, and the rollout still converges through
the one MCP generation protocol above.

A write-only Repository authorization token is different: it authors a new
Session-scoped Repository credential binding and is accepted only for an
ordinary Managed Session. Profiled rejection occurs before Vault ingress or
another material effect, while the existing Vault lifecycle remains unchanged.
Its ingress receipt independently records `Applied | Replayed` provenance beside
the Resource Registry receipt. Before Session-root adoption, compensation may
archive only an exact `Applied` source revision that the durable root did not
adopt; replayed or indeterminate work is preserved. After adoption, terminal
Session reconciliation invokes the same exact Vault archive/material reclaim
operation for an owned Repository rather than adding a credential cleanup path.

An HTTP success for update/archive/delete acknowledges the durable credential
fence and outbox commit; it is not a claim that every online Session has already
converged. Busy Sessions keep their old generation and leave the event pending
for a later rolling-reconciliation pass; immediate revocation is not provided.
Consumers that require an immediate global revocation guarantee must use a
separate short-lived execution permit or wait for an exposed convergence receipt;
the current Managed wire provides retryable rollout with exact acknowledgement. Eventual
convergence additionally assumes repeated supervisor ticks, target recovery,
and that every relevant Session eventually becomes idle; those fairness
conditions are not enforced by the outbox model or its single target adapter.

Deleting a Vault root is the composition of those child commands, never a
database cascade. The first request stores a stable root deletion identity and
revision fence, immediately hides the Vault, and rejects new child create or
update. A supervised reconciler tombstones each child through the same
Source/child/material protocol and waits until every resulting durable outbox
event has been accepted and exactly acknowledged by the configured rollout
target before completing the absorbing root tombstone. Request
cancellation or process restart therefore resumes the same operation; physical
purge of root and child tombstones is separate GC.

The repository transaction cannot include an external KMS or SecretStore.
Material may therefore exist while its durable pending fact owns it, but an
executable Source cannot exist without its exact Managed child. Recovery
publishes only a pair already fenced as `Ready`; it reclaims an expired
`Writing` attempt instead of inferring readiness from SecretStore bytes. This
uses a local transaction plus idempotent recovery, never distributed two-phase commit.
Every current-format Managed material reference admitted by the command is
namespaced with that command's stable attempt identity. Writer takeover changes
the lease owner but not that physical namespace and cannot enter `Ready`. The
Kani harness proves only
the bounded admission predicate: a current-format new-material path requires a
declared owner and a matching namespace flag. Constructor/validation tests
cover suffix construction; UUID entropy and arbitrary SecretStore behavior are
not proved. Given distinct namespaces, a fenced writer's delayed write cannot
overwrite a later attempt; it may still create an unreferenced blob. The generic
inventory reconciler detects and reports that blob while protecting both
ordinary and Managed pending facts, but does not delete it: the SecretStore and
repository have no shared transaction, and a snapshot-based delete could race a
new mutation reusing a deterministic reference. Physical reclamation therefore
requires a backend-specific durable orphan claim followed by exact deletion.
This is an application-level isolation fence, not a claim that an arbitrary
SecretStore supports conditional writes. Database-authoritative time, lease
heartbeat during a long external call, and backend conditional effects remain
adapter improvements needed to eliminate premature takeover and potentially
unbounded orphan retention before an exact collector exists.

Legacy format-0 pending rows are recovery-only; every new begin rejects them.
A rolling deployment must first stop or drain old-format writers, wait at least
their maximum writer lease plus the platform's external-write grace, and only
then enable the new recovery owner. That drain is required because legacy refs
have neither the attempt namespace nor owner proof that makes a late current-
format write harmless. Recovery may claim and abort a legacy row after the
grace, but never admits it as a new command or promotes it to `Ready`.

Recovery validates each decoded mutation before any SecretStore effect. An
unsupported future-format mutation is retained without reinterpretation or
deletion. The current SQLite/Postgres enumerators decode the batch as one
`Result`, however, so one undecodable JSON row retains the whole batch and
blocks that reconciliation pass. Per-row decode isolation plus an operator
repair signal is deferred; it requires extending the existing repository scan
contract rather than inventing a second recovery path in this change.

### Hosted application static-bearer admission

The official Vault CRUD remains the human/operator resource surface. Hosted
applications additionally need one narrow write-only command because its
random wire Vault ids cannot provide HA-safe create-or-find semantics. That
Awaken Control owns the `/v1/config/application-mcp-credentials` command, which
extends the same `VaultState`, `ManagedCredentialRepository`, `SecretStore`, and
Managed creation boundary; it is not another Vault implementation.

Control derives a stable Vault id and credential-source id from the trusted
Workspace, opaque application authority id, and canonical `McpTarget` identity.
`Idempotency-Key` identifies one material command and the required positive
`credential_generation` orders those commands. The existing deterministic
material reference carries `(command fingerprint, generation)` before its
existing writer-attempt fence; legacy references without that component decode
as generation zero. Exact current generation/key/material replay returns the
same ids and revision. A strictly newer generation rotates through the ordinary
credential WAL/CAS. An equal generation with another key or material, and any
delayed older generation, fail before a material or aggregate write, so an old
retry cannot restore superseded bearer material. The generation is caller-owned
monotonic input, not a second Awaken receipt or counter.

The response is secret-free and carries the existing
`ManagedCredentialAdoptionProgress` as `adoption: converged | pending`.
Creation has no rollout event and is converged. A rotation immediately attempts
its exact durable event through the configured rollout target; only target
convergence plus exact repository acknowledgement is converged. Missing,
unavailable, or busy targets leave that same event pending for the existing
supervisor. The request path reads that event by its existing primary event id;
only the supervisor enumerates the outbox, so request cost does not grow with
unrelated tenant backlog. Session creation then uses the returned Vault id and
the existing MCP normalizer pins that source's exact revision. A caller
waits/retries while adoption is pending; it never starts a Run with a
not-yet-adopted revision. Hosted clients never persist a local Vault/source
mirror and never fall back to an embedded credential when this command fails.

When the application must bind the bearer before Session create, it predicts
the opaque address through the Managed adapter's one exported pure Session-ID
helper. The public create path reuses that helper, while the Session repository
remains the only payload-match/replay authority. Direct retrieval and ordinary
idempotent create replace list scans or metadata-carried identity; no Session
mapping table or second receipt is introduced.

### Hosted governance Credential Resources

Flow Domain Packs describe Credential Resources and their provider-specific
presentation, but Awaken remains the only material authority. The existing
`/v1/config/credentials` collection therefore accepts an optional stable
`idempotency_key` and optional secret-free `CredentialDescriptor` for hosted
governance callers. Its identity is the exact `(Workspace, canonical provider,
idempotency_key)` tuple, where exactly one provider comes from the descriptor or
the legacy `provider_id`; new described writes reject the legacy field. Create seals material through
the existing Credential repository and SecretStore, exact replay returns the
same secret-free source, and reuse with different material fails closed.

The same POST accepts `replacement_of: {id, revision}` only for a described,
idempotent hosted create. The existing tuple derives a deterministic distinct
new source id; the existing mutation WAL atomically requires the predecessor's
full row to equal that exact reference and the new id to be absent, then inserts
the new version-1 source without changing the predecessor. Exact new-source
replay wins after publication, including after later predecessor retirement.
The new `CredentialSource` durably records secret-free
`replacement_of: {id, revision}` provenance; ordinary create records `None`, so
a pre-existing ordinary row with identical metadata and bytes cannot
impersonate a replacement replay.
Crash recovery deletes only unpublished replacement material or retains both
published generations; it never reclaims predecessor material for a cross-id
intent. Relinking consumers and retiring the old exact revision remain separate
consumer/application commands. This extends the existing repository intent and
POST route; it adds no successor store, lease, route, or parallel credential
authority. Concurrent attempts with different new ids may both satisfy the same
unchanged predecessor revision, so the business owner must serialize one
Credential Resource replacement when successor uniqueness is required.

Each descriptor declaration is one exact target identity (purpose + audience)
paired with `CredentialUsage`. Executable access serializes that target identity
and its existing usage field once. This prevents both purpose/audience Cartesian
expansion and an independent usage from passing beside a matching target. Create and material
rotation validate the material kind/type/complete field set and each declared
usage against that opened material before storage;
exact read returns the descriptor but never material. The existing
expected-version rotation publishes descriptor changes in the same Source CAS;
terminal retirement likewise requires the exact observed source revision before
it publishes disabled/archived state or reclaims material.
Subject/permission/static-expiry or target changes may retain an unchanged
material descriptor, while a material-shape change requires replacement
material in that operation. The descriptor provider is immutable after create;
changing it requires a distinct source identity. Git transport uses only canonical typed HTTP Basic;
Connector API use remains a separate scalar credential rather than a platform
invented dual-use token document.

Repository transport audiences use the credential contract's sole HTTPS-origin
normalizer: any repository under `https://github.com/...` maps to
`https://github.com/git`. The repository id/version remains in the existing
material-binding fingerprint, so reuse is host-scoped without weakening exact
repository execution binding. Different origins, userinfo, and non-HTTPS remotes
do not normalize to the declared audience.

Descriptor purpose is a closed compatibility matrix, not a free label. The
implemented pairs are Repository transport with HTTP Basic, platform HTTP
effect with the typed field-placement usage, signature verification with an
exact field-to-algorithm map, and an Extension target with the existing
Extension usage. The latter's `consumer_id` is the only consumer identity;
purpose does not duplicate it. Provider, MCP, and remote-Agent consumers derive
and freeze exact targets: catalog provider identity, canonical MCP URL, and
verified A2A origin respectively. Target-dependent access without a target is
rejected before local or external material resolution.

The targetless Secret mount/write-back adapter is legacy-only. A described
source is rejected before material is opened or changed because that wire does
not carry its exact target and usage. All material replacement, including
legacy write-back and Repository authorization updates, and every terminal
source retirement call an expected-revision Vault CAS; no read-latest rotation
or revocation wrapper remains. The application-MCP compatibility cutover is the
only same-id legacy-provider to descriptor migration and uses that same
expected-revision WAL/CAS.

The pure `compile_exact_credential_access` domain function is the only source
row-to-access projection. It validates source authority, active status,
Workspace, positive revision, expiry, target+usage, holder policy and material
binding. Exact-target consumers supply one selected holder. MCP authoring
normalizes targets before Environment resolution, then compiles credentials
with the frozen Environment `mcp_holder`; no default or deferred holder can
compete with it. HTTP and
Managed protocol adapters retain their existing row reads and envelope/custody
effects but delegate that decision instead of copying it.

`CredentialSource::validate_access_target` is the sole interpretation of a
source descriptor or legacy provider scope against a consumer target. Both the
compiler and the local Materializer call it; the latter repeats the check at
the plaintext boundary so a forged or stale access cannot bypass source-owned
authority. Provider materialization also compares an endpoint-scoped source
with the immutable versioned route, so another endpoint of the same provider
cannot reuse it. Expiry is intentionally checked at compilation and again at
materialization because time may advance between those two boundaries.

Repository activation stores the exact secret-free pin, never opened HTTP Basic
material or a short Gateway capability. Initial clone, hot replacement and
terminal publish all pass through the same Git-effect function. A Worker-held
pin is materialized against the exact Repository id/config version for that one
operation; a Platform-held pin refreshes one Gateway capability. Any intervening
disable, rotation, expiry, Workspace/target mismatch, or transport downgrade
terminates before Git I/O and cannot select another credential or holder.

The same collection performs operation lookup and idempotent-source listing;
the reference-validation subresource consumes the durable source id plus its
exact Workspace/provider and verifies hosted-source identity, active state,
current revision, Vault kind, and material presence without opening or returning
the secret. The create operation key is intentionally unnecessary after the
source id has been persisted; requiring it would force Flow to retain a parallel
identity map. This extends the ordinary Credential CRUD; it does not create a
Flow-local Vault, a second catalog, or provider-specific credential types. Flow
stores only the returned source id/revision as its Credential Resource
`backing_ref`.

This management seam intentionally does not expose a plaintext-material HTTP
operation. Execution must consume the existing exact `CredentialAccess` →
`CredentialMaterialResolver` path, including Workspace, revision, selected
holder, usage, and target-binding validation. A product-side `materialize(id,
workspace) -> secret` call would bypass those authorities and is not a supported
hosted contract; remote products must use an Awaken-mediated effect or a
recipient-bound envelope instead of retaining plaintext in their server.

The accepted hosted Connector path installs `PinnedCredentialMaterializer` in
the trusted Gateway process over Awaken's canonical `CredentialRepo` and
`SecretStore` adapters. The Gateway selects its exact opaque Platform holder
and `PlatformRelay`, resolves the pinned source only against the route-owned
Workspace and effect-target fingerprint, and admits the built-in
`CredentialUsage::HttpEffect`. That usage maps each material field to exact
header, query, or RFC 6901 JSON-pointer destinations. A single secret requires
one sole field, structured material requires an exact declared-field match, and
OAuth fails closed. The Gateway also compares the effect's actual reference set
with the frozen usage before substitution and performs the effect in that same
process. Flow receives only a bounded upstream result plus a secret-free
receipt. Process separation therefore requires neither a plaintext material RPC
nor a copied Vault.

`CredentialEnvelope` is the single recipient-bound transport contract. The
canonical Vault compiler optionally delegates the plaintext-to-ciphertext step
to a deployment-owned `CredentialEnvelopeIssuer` only after exact source,
Workspace, revision, holder, usage and target binding validation. Open
self-hosted composition installs no issuer. A hosted implementation owns its
KMS, ciphertext transport and Worker-private resolver, while reusing the same
`CredentialAccess`, payload fingerprint and pinned materializer; failure cannot
fall back to an unsealed hosted reference. Deployments without either the
in-process stores or that reviewed transport fail closed.

The vault ACL inside `awaken-protocol-managed` maps this wire ⇄ the neutral domain below; the
domain's `CredentialAuth` variants stay neutral (`Bearer`/`OAuth`/`EnvVar`/`ApiKey`),
the wire keeps the Managed tags.

## SecretStore Port

Materialization lives behind a port; encryption-at-rest is one adapter choice, not
a domain concern and not a service you must run (a `Vault` is tables + this port,
no daemon):

```rust
trait SecretStore: Send + Sync {
    async fn put(&self, r: &SecretRef, s: RedactedString) -> Result<()>;
    async fn get(&self, r: &SecretRef) -> Result<RedactedString>;
}
```

Adapters: `inmem` · `plaintext-file` (dev) · `sealed-aead` (key from OS
keychain/KMS, **physically separate from the ciphertext**) · `external-kms` /
`hashicorp`. A separate `secretd`/egress-proxy component is warranted only by
multi-node rotation authority, blast-radius isolation, or secretless sandboxing,
and always slots behind this same port.

## Security Layers

| Layer | Mechanism | Defends against |
|---|---|---|
| Type system | `RedactedString` (redacted, zeroize, non-serde), single `expose_secret()` | accidental serialization/logging; memory residue |
| Secret-free aggregates | config holds only a `CredentialBinding` (source/pool id); `CredentialSource` holds only a `SecretRef` | leakage via snapshots, wire, audit, replication |
| At rest | `SecretStore` sealed-AEAD with a separate key; or an explicitly persisted OAuth helper mints a short-lived token | stolen disk / DB dump / backup |
| In use | materialize at injection seam only; `Networking.allowed_hosts`; sandbox can't see vault | compromised/malicious agent exfiltration |
| Access control | `workspace_id` scoping + `credential.*` authz (ADR-0042) | wrong-tenant read/use |
| Blast radius | optional `secretd`/egress proxy; process-group reap | master-key exposure; residual-process leak |

Must-enforce: no inline/ambient-secret-in-config (an executable secret always lives in a
persisted `CredentialSource`); AEAD keys physically separate from
ciphertext; `credential.*` authz is required.

## Egress-Proxy Delivery (Managed, Untrusted Agents)

For untrusted third-party agent code, point `ProtocolEndpoint.base_url` at a
credential-injecting egress proxy that holds the credential and enforces
`allowed_hosts`; the agent becomes **secretless**. This is another
`SecretResolver`/delivery adapter — the runtime seam is unchanged. It is strictly
more secure for network credentials **but only if egress is network-locked to the
proxy** (sandbox netns/firewall); it composes with, and does not replace, network
isolation.

ADR-0066 limits the first convergence target to MCP and gates feature coding on
contract-closure Slice 0. A scoped repository envelope first persists a consumed
Session preparation intent. A claim-time application contributes one secret-free
plan back to Control; the sole compiler then freezes the baseline and generation
1 Resource/MCP state. Published Agent MCP, Session `vault_ids` compatibility,
and application MCP normalize once into exact credential access. Later Managed
`agent.mcp_servers` full replacement diffs into the same attachment set rather
than rewriting a wire-only projection. Model and Repository remain in their own
aggregates. There is no generic `ServiceDefinition`, inference Service
projection, or public Service realizer in the first slice.

A Worker-local relay is the default mediated MCP adapter. The public
`McpAttachmentRealizer` Session port is also injectable by a downstream
deployment that returns a gateway endpoint and opaque lease. Injection replaces
the local adapter for the exact stage/publish/drain path; an injected failure
never falls back to local materialization. The relay remains a private Runtime
Host implementation detail, and Awaken never imports downstream gateway, route,
IAM, or Vault-backend types. The Session's frozen `EnvironmentSnapshot.network` is
authoritative: adding or replacing MCP may change a route behind an already
admitted stable endpoint, but may not expand the live Sandbox allowlist. Direct
access outside that policy fails closed and requires explicit Environment
migration.

The existing `SecretBroker` remains the one file materialization/write-back
port. It is not widened into a network proxy. A separate neutral
`CredentialMaterialResolver` consumes one exact request containing
`CredentialAccess`, selected holder, trusted Workspace, and a canonical
target/use fingerprint; it cannot enumerate or select. Its capability evidence
separately declares material sources and recipient-bound envelope support. The
canonical Worker materializer handles unsealed Control references locally and
delegates Worker references or envelopes to one installed resolver without
fallback. Exact
`CredentialRefreshAccess` preserves OAuth refresh/reseal without URL or
current-Vault rediscovery. Target binding, streaming lifecycle, and
sandbox-facing MCP route projection remain Runtime Host adapter
responsibilities. Failure never authorizes plaintext in another trust domain.
`awaken-credential-materializer` contains the sole `CredentialRefreshFactory`
and Vault-backed refresher. Runtime Host consumes that factory port; Coordinator
does not own or duplicate refresh policy, token exchange, or Vault write-back.

In a split deployment, the implemented self-hosted path is the external
Secret/CSI projection described below; brokered inference remains secretless to
the Worker. There is no `ControlCredentialMaterialResolver` HTTP implementation
and no plaintext credential response DTO. AllInOne injects the local Vault
implementation through the same resolver port. Neither composition may
enumerate credentials, choose another revision or holder, or fall back to a
database after a projection or broker failure.

Credential material and last-mile consumers are open to external extensions
without adding protocol variants to the core enum. A structured Vault document
contains a namespaced, versioned `type_id` plus opaque named secret fields;
`CredentialUsage::Extension` pins the exact `consumer_id`, expected material
type, and explicitly secret-free `public_config`. Installed Worker capability evidence
must advertise that exact consumer/material pair before admission. The external
consumer implements `CredentialExtensionConsumer` and receives only the already
selected material, access, and target-use binding. SSH is therefore an extension
(for example `acme.ssh-key/v1` + `acme.ssh-agent/v1`), not a permanently built-in
credential kind. Built-in process/file delivery continues through `SecretBroker`,
and external material sources continue through `CredentialMaterialResolver`; the
extension consumer does not replace or duplicate either port.

## Credential Custody and Model Exposure

[ADR-0067](../adr/0067-credential-custody-model-exposure-and-secret-delivery.md)
defines explicit allowed plaintext-holder trust domains, the separate
model-exposure policy, evidence requirements, and no-holder-fallback behavior.

Vault describes storage at rest, not execution custody. Published access lists
the exact Workload, Worker, or Platform trust domains allowed to hold plaintext;
those holders are not ordered. Unsupported capabilities, route failure, or lease
loss fail closed rather than selecting another holder.

Model exposure is orthogonal and initially closed to `Forbidden` and
`VirtualOnly`. A value that looks like a credential may be model-visible under
`VirtualOnly` only when it is synthetic, target/Session/generation scoped,
expiring, and substituted at the Worker/platform boundary. Real material in an
ACP process environment can coexist with `Forbidden` model exposure.

The Worker MCP relay uses that same scoped-capability shape internally: an ACP
Sandbox receives only a randomly generated exact-generation route URL. The
private route binds target and real material, checks lease expiry on every call,
and is removed by replacement/drain/terminal cleanup. Predictable route URLs and
post-expiry forwarding are rejected; the capability is not persisted or emitted
in receipts/events. This proves the mediated transport primitive but does not by
itself authorize model exposure—`VirtualOnly` must still be explicit on the
published access policy.

`McpRelay` is a private Runtime Host effect adapter, not a public security or
Session abstraction. `McpAttachmentRealizer` is the only public MCP effect port,
and local/remote adapters share the same Session realization phase driver. Lease
renewal is issued by that driver under the existing owner/incarnation/epoch;
successful publication updates the route expiry without changing its synthetic
capability. Worker heartbeat or renewal authority loss invokes terminal Host
disposal for all local Session projections. The relay cannot self-renew, retain
material after authority loss, or become a second attachment registry.
Authenticated relay routes are created only while staging through that port.
Runtime construction consumes the already-published virtual endpoint and may
not lazily start the relay or repair a missing route; restart recovery must
rehydrate it through the same durable stage/publish protocol.
The implemented MCP adapter consumes only canonical Authorization Bearer usage
and verifies that exact `CredentialUsage` before opening material; it never
coerces another published usage into bearer authentication.

`CredentialAccess` and existing `CredentialUsage` are extended with a material
source, optional recipient-bound sealed payload reference, one exact material
resolver, optional exact OAuth refresh/reseal access, and execution policy; no
parallel `CredentialDelivery` policy is added. The frozen Environment/execution
profile requests separate exact allowed holders for inference, MCP, and Resource
execution. MCP persists its holder with the attachment generation; a resolved
Repository input persists its exact access and Resource holder beside the pinned
Repository config version; and the dispatch claim transaction persists Model
execution's `AttemptCredentialBinding` atomically with Worker/lease epoch. All
three paths use the same exact material resolver. Runtime cannot open a bare
Repository Vault source id or perform a second source/revision selection. The
binding stores the planned mechanism; a secret-free receipt stores the actual mechanism.
Explicitly non-durable local Runs reuse the same binding compiler but fence the
binding to their process-local active Session `run_id`; they have no recovery or
reassignment promise and cannot synthesize a durable dispatch claim. Losing that
run ownership rejects both materialization and receipt recording.
Legacy `Direct` provenance survives serialization and durable queue/database
round trips, so compatibility decoding cannot launder it into an admissible
Control reference. Broad Worker selection preflights the same authoritative
credential admission kernel and skips an incompatible row; an exact claim keeps
the explicit error. That preflight decodes the contract-owned frozen Session
runtime projection and admits every MCP generation's exact access/holder pair
against the selected Worker's installed capabilities through the same
`CredentialAccess::admit` kernel. The Session generation remains the holder and
receipt authority; dispatch does not persist a parallel MCP attempt binding.
This prevents one incompatible high-priority row from blocking later valid work
without creating a second selection policy.
`ResolvedModelCandidate` remains the only Model access authority, and Session
`vault_ids` never override it. Automatic LLM Vault authoring is deferred until a
separate proposal defines its application service, transaction/saga,
idempotency, compensation, outbox, and orphan cleanup.

### Distributed Worker-private material

The server-role Worker does not open the Control credential repository or
`SecretStore`, and Awaken does not serialize `RedactedString` into a private HTTP
response. `WorkerCredentialFileResolver` is the production boundary adapter for
an exact `CredentialMaterialSource::WorkerReference` and a recipient-bound,
externally projected `ControlPlaneReference`. An external Secret/CSI provider
projects Worker-local material under:

```text
<root>/<hex credential id>/<revision>/<hex workspace>/<hex target-use fingerprint>/
  secret                 # scalar Provider/header/query/env/file usage
  username + password    # canonical awaken.http-basic/v1 only
```

For a Control envelope, the adapter uses a distinct exact path:

```text
<root>/envelopes/<hex envelope id>/<hex payload fingerprint>/
  <hex credential id>/<revision>/<hex workspace>/<hex target-use fingerprint>/
  payload_fingerprint
  secret | username + password
```

The adapter supports one configured Worker `PlaintextHolder`. A Control
reference is accepted only with a Worker envelope whose recipient and expiry
are live and whose projected marker equals the pinned payload fingerprint.
Another trust domain or a changed envelope id, payload, revision, Workspace,
target, or usage resolves to a different or absent path and fails closed. The
adapter never enumerates the root or falls back to another revision/binding. The
files are plaintext only inside the selected Worker trust domain and must be
supplied and protected by the deployment's Secret/CSI volume policy.

AllInOne continues to use the local `PinnedCredentialMaterializer` over the
Control-owned stores. Both compositions consume the same exact resolver port and
the same Model/MCP/Repository validation. The published material source remains
authoritative; deployment composition installs the matching adapter without a
compatibility conversion or database fallback.

### Dormant Sandbox control transport boundary

The provider-neutral Sandbox control contract exposes one pre-create-known
logical coordinate,
`/run/awaken/control/repository-git-credential.sock`, and a bounded typed
request/response channel. Linux Namespace projects that directory read-only from
a provider-private host rendezvous. Kubernetes projects one private `emptyDir`
read-only to the Agent and writable to a digest-pinned loopback forwarder; a
same-binary exec probe checks a current generation marker without consuming the
business channel. Exact realized topology and, for Containers, the provider
incarnation survive in the durable Sandbox handle so adoption cannot silently
downgrade a demanded control service.

This is transport, not a credential holder or activation policy. The provider
does not choose an access pin, holder, exposure, target, claim, verifier,
Repository, or Vault revision, and it does not create `GIT_CONFIG*`, a Git
helper overlay, or an Agent-visible synthetic credential. The current
Repository access pin remains `Forbidden`. A later Session-owned projection may
refer to the stable coordinate when it compiles the complete sandbox spec, but
it must first reuse the existing exact access/holder/material authorities and
must activate Agent plus Native/Resident-Hand paths from that one projection.
Transport availability alone authorizes nothing and is not evidence that this
later activation has landed.

## Staging

Keep one authority per responsibility and stage additional policy without adding
parallel aggregates:

- **P0 (single-machine):** `CredentialSource` (kind `Vault`) +
  `SecretStore` (inmem/sealed durable adapter) + `CredentialBinding::Exact` only.
- **P1 (managed):** `CredentialPool` + `CredentialBinding::OneOfCredentialPool` +
  `CredentialSelectionPolicy`; `InferenceProfile.disabled_endpoint_ids` and
  immutable `CredentialSource.protocol_endpoint_id` proof scope;
  sealed-AEAD `SecretStore`; Managed ACL; `credential.*` authz + workspace scope.
- **P2:** generalized OAuth/background rotation beyond the existing MCP OAuth
  refresh/reseal path, `Networking.allowed_hosts` / `injection_location`
  enforcement, `CredentialValidation` audit, egress-proxy secretless delivery.

## Guardrails

G8 and G9 in [INVARIANTS](../INVARIANTS.md).
