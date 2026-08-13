# ADR-0043: Management/Runtime Boundary and Resolved-Secret Runtime Contract

- Status: Proposed
- Date: 2026-07-04
- Amended 2026-07-30: a direct Provider connection defaults one protocol surface
  to `(provider_id, dialect)` and uses an explicit endpoint name only to qualify
  additional surfaces speaking the same dialect. The application derives the
  stable endpoint id; clients do not author a parallel id namespace. Credential
  materialization, selection, target usage, and execution policy remain separate
  existing facts.
- Amended 2026-07-29: provider connection is one reusable application command,
  and executable model readiness is one server-owned
  Catalog/Credential/Brokered-access projection shared by HTTP and embedded
  hosts.
- Amended 2026-07-30: the official Managed `model` string is decoded into
  unresolved selection intent; no metadata key selects execution. One Offering
  selector and the executable model directory project the same catalog facts.
- Amended 2026-07-04: provider/endpoint/offering/routing live in a **dedicated
  management-plane crate `awaken-management-contract`** (mirroring awaken-next
  ADR-0088), orthogonal to execution (execution never depends on it — I4 = D6/D9);
  names align to that crate (`ProtocolEndpoint`/`Offering`/`InferenceProfile`/
  `InferenceTriple`/`ModelApiCompat`/`model_ref::ResolvedModel`), not raw
  oversight-next; the Managed API's `model` resolves through the model-id ACL and
  publication planner.
- Relates to: [ADR-0038](0038-managed-resource-injection-and-store-organization.md),
  [ADR-0039](0039-runtime-persistence-port-convergence-and-store-naming.md),
  [ADR-0042](0042-public-api-tenancy-authz-and-front-door-consistency.md)
- Model detail (deliberately outside this ADR):
  [model-provider-backend-binding](../design/model-provider-backend-binding.md)
  (agent/model/provider specs + model intrinsic attributes),
  [credentials-and-vaults](../design/credentials-and-vaults.md)
  (vault/credential/`SecretStore`/security/proxy),
  [config-to-run-execution-flow](../design/config-to-run-execution-flow.md)
  (routing-graph → `SecretInput` projection and injection)

## Context

Agent configuration, model catalog, provider settings, and credentials/vaults are
**management-plane** concerns. The runtime executes a resolved, self-contained
snapshot and must stay **completely unaware** of how that snapshot was authored,
versioned, tenanted, or where its secrets live.

This ADR records **one decision**: the management/runtime boundary and the fact
that only **resolved** secret values cross it (the resolution seam lives in the
host, not the runtime). The concrete data model — goal's `AgentSpec`/`ModelSpec`
plus the **`awaken-management-contract`** provider/routing/credential graph
(`ProtocolEndpoint`, `Offering`, `ProviderCatalog`, `CredentialBinding`,
`InferenceProfile`, `InferenceTriple`, `model_ref`) reusing
the Managed wire via an ACL, `SecretStore` adapters, and the security layers —
lives in the design docs above, because that model will churn (auth variants,
pool/routing, validation, egress proxy) while this boundary must not.

Two hard requirements frame the decision:

1. **Runtime is unaware of the management plane.** No vault schema, credential
   CRUD, OAuth/refresh, tenancy, or public API shape reaches a runtime crate
   (continues the boundary in [credentials-and-vaults](../design/credentials-and-vaults.md)).
2. **The runtime accepts either a *normal config* or a *vault-backed config*
   through one identical entrypoint.** Both arrive **already resolved**: "normal" =
   the credential is literal material; "vault-backed" = the **host** resolved the
   binding/handle upstream. The runtime never resolves and never sees the
difference — matching the existing D6/D9 boundary (`awaken-ext-mcp::Credential`
takes already-resolved values; resolution stays in the host).

**Amended by ADR-0062:** the durable snapshot now carries secret-free,
fingerprinted `ResolvedModelCandidate` values whose provider provisioning composes
the existing `CredentialAccess`, not a `RedactedString`.
Configuration publication selects the exact reference once; the host execution
adapter materializes it immediately before constructing/calling the provider
executor. Runtime core still links no vault/catalog and never performs selection.
This supersedes later references in this ADR to a materialized credential value
inside `ExecutableAgentSnapshot`.

## Decision

### 1. The management plane owns the model; the runtime consumes a compiled snapshot

Management-plane crates author, version, tenant, and **resolve**:
`awaken-agent-contract` (specs), **`awaken-management-contract`** (provider
catalog / endpoints / offerings / inference routing — its own config API, ADR-0088,
orthogonal to execution, **execution never depends on it, I4**), `awaken-credential-vault`
(vault/credential), and `awaken-protocol-managed::control::vault_acl` (Managed wire
ACL). The former `awaken-managed-bridge` was consolidated into the sole Managed
protocol anti-corruption boundary; no compatibility crate remains. The runtime links
**none** of them. It receives a compiled `ExecutableAgentSnapshot`/`RunnableConfig`
in which every credential is an **already-resolved value** (a `RedactedString`), or
absent (host-native / egress proxy). Dependency direction is management plane →
`awaken-runtime-contract`, never reverse.

The **Managed Agents API consumes the management plane by reference, not by
value**: its official `model` string is decoded by the sole Managed model-id ACL
into unresolved `ModelSelection` intent, resolved by the sole Offering selector,
and admitted against Provider, credential, executor, and Worker capabilities.
`metadata.awaken.model` is not an execution path. The Workspace-scoped
`/v1/models` directory is a rebuildable projection of the same executable facts;
write paths always revalidate and freeze an immutable candidate — see
[model-provider-backend-binding](../design/model-provider-backend-binding.md).

### 2. The runtime receives resolved secrets; the host owns the seam (the durable decision)

The runtime kernel and extensions receive **already-resolved** secret values —
never a handle, a vault ref, or a resolver. This is the existing **D6/D9** boundary
(`awaken-ext-mcp::Credential` carries an already-resolved value; *"the host resolves
its secrets and hands this crate the opaque value, keeping credential mechanics —
vaults, OAuth refresh — out of the runtime/extension boundary"*). Secret-lifecycle
vocabulary does **not** appear in `awaken-runtime-contract`.

- **`awaken-runtime-contract` + extension crates** carry at most `RedactedString` as
  an opaque, already-resolved credential value on the resolved provider config they
  consume. No `SecretHandle`, no resolver, no `Literal`-vs-`Handle` union.
- The **host / integration layer** (server/serverd/dispatch assembly, depends on
  `awaken-credential-vault`) owns the seam. It resolves the full routing graph —
  an `InferenceProfile` candidate's exact target + `CredentialBinding` (`Exact`
  or pool) → endpoint-compatible `CredentialSource` → `SecretStore` (see
  [credentials-and-vaults](../design/credentials-and-vaults.md),
  [model-provider-backend-binding](../design/model-provider-backend-binding.md)) —
  and **materializes the secret before building the runtime snapshot.**

```rust
// host / integration layer — NOT awaken-runtime-contract
enum SecretInput { Literal(RedactedString), Handle(SecretHandle) }
trait SecretResolver: Send + Sync { async fn resolve(&self, h: &SecretHandle) -> Result<RedactedString>; }
```

`RedactedString` (`secrecy::SecretBox` + zeroize + redacted `Debug`/`Display`; sole
accessor `expose_secret()`) is the one value type that crosses into the runtime.

**Lazy / rotating resolution is a provider-adapter concern, not the kernel's.** If a
long run must re-fetch an expiring token, the concrete adapter (e.g.
`awaken-provider-genai`) takes an injected credential-source trait it calls per
request; the kernel's `ModelProvider`/`LlmExecutor` port stays secret-free and the
adapter is wired at the composition root. So no functionality is lost — the seam
simply lives above the kernel contract, not inside it.

### 3. One persisted credential path — selected before, materialized after snapshot

Configuration publication resolves a persisted `CredentialBinding` into a
secret-free, revisioned `CredentialAccess` pin in the snapshot. Worker/host
provisioning opens that exact persisted source immediately before constructing the
provider/ACP adapter. The runtime kernel never selects or resolves a credential.

There is no parallel "normal/env config" execution path. A secret-free environment
proposal must first be accepted through the ordinary Catalog/Credential authoring
commands; until then it cannot be published or executed.

### 4. Hard constraints

- **No inline/ambient-secret-in-config** — a secret always lives in a persisted
  `CredentialSource` (vault or persisted OAuth helper), never embedded in a spec or
  read from provider environment at execution time; the
  secret-free invariant holds on every profile.
- **A missing handle fails closed** — no ambient env fallback.
- **`credential.*` authz is required** (ADR-0042); the draft credential domain has
  none by itself.
- Encryption-at-rest and secretless egress-proxy delivery are `SecretStore`/
  `SecretResolver` **adapters** in the host/credential layer, never runtime or
  domain concerns (see the design docs).

Environment values may be inspected only by the admin proposal adapter; they must be
explicitly entered into the vault/catalog and published before any run can consume them.

[ADR-0067](0067-credential-custody-model-exposure-and-secret-delivery.md)
defines orthogonal execution-time plaintext-holder and model-exposure
requirements. Vault persistence is not a custody grade: an exact credential may
be opened by a workload, Worker, or downstream platform only when that exact
trust domain appears in the published allowed-holder set and the frozen
Environment and installed capabilities admit the mechanism. The holders are not
ordered, and no adapter may switch trust domains after failure. ADR-0067 also
preserves ADR-0062's `ResolvedModelCandidate` as the sole inference authority and
defers automatic LLM Vault authoring to a separate application-workflow design.

For avoidance of doubt, ADR-0062 supersedes this ADR's sections 1-3 wherever
they place a materialized `RedactedString` inside an inference executable
snapshot, and ADR-0067 supersedes their single-host delivery assumptions. The
authoritative inference snapshot is secret-free `ResolvedModelCandidate` plus
exact `CredentialAccess`; the atomic dispatch claim-epoch record pins the
selected plaintext holder and planned realization before materialization, and a
secret-free receipt records the actual mechanism. The selected adapter
materializes only at the last supported boundary. The
historical simple-design and staging text below remains background for the
original proposal, not current inference implementation guidance.

### 5. Provider model discovery reconciles the existing catalog; it is not runtime resolution

An operator may ask an authored `ProtocolEndpoint` to list models using one exact,
Workspace-owned `CredentialSource`. The admin application passes those two
secret-free facts to a provisioning port. Its provider adapter materializes the
credential at that seam, follows every provider pagination cursor, and returns only
normalized model ids. The application then atomically reconciles the result through
the existing `CatalogRepo`; there is no discovery cache or second model directory.

Discovered offerings carry provider provenance and an active/unavailable admission
status. A complete later listing marks missing provider-owned rows unavailable
instead of deleting them, while explicitly authored offerings always win and are
never demoted by discovery. Only active offerings may enter a new publication;
already-published immutable candidates remain explainable and executable according
to their own pins. A failed, partial, malformed, or unconfigured discovery performs
no catalog mutation. Worker-private material requires a worker/provisioning adapter
that owns that reference; a control-plane adapter cannot fall back to environment
variables or request the plaintext.

`ProviderConnectionService` is the one application-service owner of this use
case. HTTP and embedded hosts perform scope/authentication and map their DTOs,
then call the same command; they do not recreate provider, credential, discovery,
or catalog orchestration. Provider-driver descriptors own authoring fields, and
the service owns provider-specific endpoint construction, so clients submit
non-secret configuration values instead of duplicating driver URL rules.

### Hosted model supply is a capability posture, not a second catalog

The existing `ConfigCapabilitiesView` / `ModelSupplyCapabilityView` is the one
deployment posture contract for model supply. A self-hosted process advertises
local Catalog authoring and BYOK; a local process signed in to Awaken Cloud may
advertise those capabilities together with brokered models. A hosted product
composition advertises only brokered Cloud models:

```text
local_catalog_enabled      = false
byok_enabled               = false
cloud_models_enabled       = true
profile_authoring_enabled  = false
```

These values are both a client capability projection and a server-side command
gate. A hosted UI hiding a Provider form is insufficient: the Provider
connection command, manual model-attribute writes, Provider-scoped credential
entry, manual brokered refresh, and inference-Profile authoring fail before
secret decoding or persistence. Generic non-model credentials remain available;
the posture does not turn the Credential/Vault context into a model-only store.

The Cloud model list enters through the existing `BrokeredCatalogDiscovery`
port and reconciles the existing Catalog as a rebuildable projection. Runtime
publication still uses the injected `ModelPublicationResolver`, so the
projection cannot become a second route authority. Hosted composition owns no
Provider connection command, no alternate resolver, and no hidden fallback.
It exposes Provider-native model ids and explicit candidate ordering only.

Management authorization separates model-supply reads and mutations from
ordinary Workspace configuration. The product-owned profile defines
`model_supply.read`, `model_supply.connect`, and `model_supply.write`. The
ordinary self-hosted `workspace_admin` retains model-supply administration;
the dedicated hosted Workspace role receives ordinary Workspace actions plus
`model_supply.read` only. Deployment binds roles but cannot redefine these
actions or their Workspace scope.

For a direct Provider connection, the unnamed `ProtocolEndpointId` is the
canonical `<provider_id>.<dialect>` projection. The request selects a dialect but
does not mint an endpoint id. Reconnecting the same unnamed Provider/dialect
surface updates it; selecting another dialect creates the other surface. When a
Provider genuinely exposes multiple endpoints using one dialect, the request
adds `endpoint_name` and the service derives
`<provider_id>.<dialect>.<endpoint_name>`. A legacy `endpoint_id` request member
is decode-only compatibility input and has no authoring authority. Brokered routes
retain their internal opaque route identity: they are a managed model-supply
projection, not another direct Provider connection namespace.

This endpoint identity does not collapse credentials into protocol data.
`CredentialKind`/material origin owns how material is obtained,
`CredentialBinding` owns which source is selected, and the published
`CredentialUsage` owns how the exact Native/ACP/remote consumer receives it.
Compatibility is checked when these facts are joined; no descriptor or endpoint
defines a second credential-usage vocabulary.

For API-key and OAuth authoring, the initiator supplies a stable idempotency key.
The Credential/Vault context derives one command-owned source identity and its
material reference, returns the durable winner on replay, and rejects conflicting
non-secret facts. The resulting source records the exact canonical endpoint it
proved; provider-wide credentials authored outside this connection flow keep no
endpoint restriction. `InferenceProfile.disabled_endpoint_ids` remains routing
policy and cannot widen that immutable proof scope. The application discovers
before persistence, stages a newly
entered source as disabled, atomically reconciles Catalog facts, then activates
the source. Catalog rejection therefore leaves no executable offering and only a
disabled, retryable credential source. Replaying a completed command reuses the
same source and reconciliation path; there is no transport-owned retry store or
second credential-create path.

Selection clients read `ExecutableModelOption`, the authoritative read model
joining current Catalog offerings with Workspace Credential availability. The
projection reports readiness but grants no authorization and never selects a
runtime fallback. Clients must not reconstruct that join from separate catalog
and credential endpoints.

## Management-plane decomposition, layering, and naming (amended 2026-07-04)

### Three layers

- **L1 — external API (only externally visible):** the Managed Agents wire
  (`awaken-protocol-managed`, Anthropic-compatible) + the admin config API
  (`awaken-admin-config-api`, self-hosted-only knobs Anthropic's wire lacks:
  provider/endpoint/inference-profile).
- **L2 — management plane, two faces:** *config stores* (authoring, external CRUD)
  **and** a *resolver service* (internal, queried by ingress — never an interface).
- **L3 — internal support stores (not externally visible):** commit/dispatch/file/
  memory/skill — ports + backends only.

### Split rule

- operator configures it → **config store** (has an API);
- execution/persistence bytes → **internal store** (no API; file/memory surface
  only as *runtime resources* via the front door, ADR-0038 — never as config APIs);
- combines config → executable → **resolver service** (no API, owns no aggregate).

Not everything in L2 is an interface: only the config stores are; the resolver is
queried, not exposed.

### Prefix = domain SSOT + split/merge unit

Each context owns one prefix across its crate (`awaken-<prefix>`), migration
namespace (`<prefix>`), tables (`<prefix>_*`), and (for external ones) route base.
One `MigrationBundle` per prefix (as today's `config`/`commit`) makes deployment
**mergeable or splittable with zero schema change**: apply all bundles to one DB
(single service) or move a context to its own DB/service. **Default: one service**
(`awaken-coordinator-local` assembly); **credential is the first split candidate**
(blast-radius / `secretd`).

### Store vs resolver (DDD service vs aggregate)

The three domain crates own their aggregates and repository ports; their `*-store`
adapter crates own durable SQLite/Postgres/encryption mechanisms. The resolver owns
**none** — it reads the injected ports and emits an `ExecutableAgentSnapshot`. The
earlier "inference domain" was really a resolver mislabeled; `InferenceProfile`
(routing policy) is an aggregate and lives in `awaken-model-catalog`.

### Intent-revealing crate names

| Purpose | Authoritative crate | Durable adapter |
|---|---|---|
| provider/endpoint/offering/model catalog + `InferenceProfile` routing policy | `awaken-model-catalog` (domain) | `awaken-model-catalog-store` |
| vault/credential/binding/pool/identity + materialization use cases | `awaken-credential-vault` (application) | `awaken-credential-store` |
| agent config authoring/publish/compile + repository ports | `awaken-agent-config` (domain) | `awaken-config-store` |
| read all config → `ExecutableAgentSnapshot` (`InferenceTriple` + `MaterializedCredential`) | `awaken-config-resolver` *(was the misnamed `awaken-inference`)* | resolver **service** — no aggregate, no execution |
| run the model | `awaken-provider-genai` | execution |
| Managed Agents wire | `awaken-protocol-managed` | front door |
| Managed wire ⇄ domain ACL | `awaken-protocol-managed::control::vault_acl` | protocol ACL |
| admin config API (provider/endpoint/inference-profile) | `awaken-admin-config-api` | API assembly |
| the one service | `awaken-coordinator-local` | assembly |

Rule: **name = responsibility, not layer.** "inference" is reserved for where
inference actually runs (`awaken-provider-genai`); "resolver" for config→executable;
the `-store` suffix for persistence adapters; "catalog" for the model registry.

### API contract & generated TS client (like oversight-next)

The **admin config API** (`awaken-admin-config-api` — provider/endpoint/offering/
model/inference-profile, which the Anthropic wire does not define) ships a
**generated TS contract**, mirroring oversight-next's pipeline:

1. Management DTOs and secret-free projections derive `schemars::JsonSchema` behind
   a `schema` feature (the convention `awaken-api-contract` already uses — its
   `ApiError`/pagination/filter/`CredentialRef` primitives come in for free when
   the feature is enabled).
2. `export_openapi` / `export_schemas` examples (per oversight-next) dump OpenAPI +
   JSON Schema; a `scripts/contract/generate-contracts.sh`-equivalent runs a TS
   generator → committed `*.d.ts` types + a typed client.
3. A CI **drift test** regenerates and diffs, failing if the committed TS is stale.

Scope: **we generate TS only for our own surfaces** (admin config API + any
non-Anthropic front doors). The **Managed Agents wire is NOT generated** — its TS
contract is the **official `@anthropic-ai/sdk`**; the single
`awaken-protocol-managed` anti-corruption boundary conforms to it (never regenerate
Anthropic's SDK).

### Layer map (resolves the G7 naming debt — no rename of existing code)

Authoring names (management plane) → resolve/compile → resolved names (existing
runtime types). They are **two layers, not duplicates**:

| authoring (management plane) | via | resolved (existing runtime) |
|---|---|---|
| `AgentConfig` | `compile` | `ExecutableAgentSnapshot` |
| `InferenceProfile` / catalog / `model_ref` | `resolve_inference` | `InferenceTriple` → `ModelBinding` |
| `CredentialBinding` | materialize | `MaterializedCredential` / `RedactedString` |

Management-contract names live in the management plane; `ResolvedSpec`/`ModelBinding`
stay at the resolved boundary the runtime consumes. No existing type is renamed
(honours "state reason before rename").

## Simple design and DDD (of this boundary decision)

- **Reveals intent / no duplication:** the runtime-facing secret story is one value
  type (`RedactedString`); resolution (handle/pool/vault/proxy) is expressed once,
  in the host layer, and never leaks into the runtime.
- **Fewest elements:** `awaken-runtime-contract` gains **zero** secret-lifecycle
  types or ports — at most it carries a `RedactedString` value. All richness stays
  management/host-side. (Consistent with the existing D6/D9 boundary.)
- **Hexagonal / DIP:** the `SecretResolver` port lives in the host/integration
  layer with `awaken-credential-vault` adapters; the provider adapter, not the kernel,
  owns any lazy/rotating credential-source trait.
- **Secret-free domain:** runtime snapshots carry only already-resolved
  `RedactedString`; plaintext exists only at the injection seam, zeroized on drop.

## Staging

- **P0 (single-machine):** runtime gets a resolved `RedactedString`; host resolves a
  single credential via `CredentialBinding::Exact` + a single resolved
  `ProtocolEndpoint` (`resolve_inference`). No handle, no pool, no failover.
- **P1 (managed):** host-layer `SecretInput::Handle` + injected `SecretResolver`
  backed by `awaken-credential-vault`; `CredentialPool`/`OneOfCredentialPool` +
  multi-candidate `InferenceProfile` failover; `credential.*` authz. (Runtime
  unchanged.)
- **P2:** egress-proxy secretless delivery as another `SecretResolver` adapter;
  provider-adapter lazy/rotating credential source; `ProviderDiscovery` backfill.

## Non-goals

- No vault schema, credential CRUD, OAuth/refresh, tenancy, or public API shape in
  any runtime crate.
- No ambient env fallback for an unresolvable credential (fail closed).
- No secret-lifecycle types (`SecretHandle`/`SecretResolver`) in any runtime or
  extension crate — that would re-break D6/D9.
- No product policy in store/repository implementations.
- No separate secret service required for single-machine (P0/P1).

## Consequences

- The runtime stays a self-contained execution engine that only ever holds
  resolved `RedactedString` values; management-plane evolution (vault, tenancy,
  rotation, pool, proxy) never forces a runtime change.
- One host-layer `SecretResolver` seam covers literal, env, vault, and egress
  proxy — swappable at the composition root; the runtime is blind to the choice.
- The credential/vault/model data model is owned by the three design docs and can
  churn without reopening this ADR.
### 2026-08-03 amendment — the console projects composed product surfaces

`ConfigCapabilitiesView` is also the sole browser discovery contract for
process-owned product surfaces. The composition root projects whether the
same-origin server owns Managed runtime/resources; the control plane projects
whether its configured IAM adapter owns access-token administration. The
browser derives its rail, command palette, settings links, assistant entry, and
direct-route recovery from those facts.

A split `Control` process therefore exposes Agent/model authoring but does not
advertise Session, Environment, Deployment, Skill, Memory, File, Artifact,
Vault, protocol, A2A, or runtime-assistant surfaces owned by
`Coordinator`/`AllInOne`. A local `AllInOne` process continues to advertise
them. Remote Cloud IAM does not advertise the embedded-IAM token-management
page. A hidden page is not an authorization decision: every mounted backend
route retains its existing PEP.

This capability projection replaces two invalid alternatives: presenting
known-unmounted pages until their requests return `404`, and adding a Cloud
proxy that imitates the open Managed APIs over HostedRun. Cloud may link to its
own Hosted execution product, but it does not become a second implementation of
Awaken Sessions or resources.

### 2026-08-13 amendment — brokered readiness follows the existing access path

`project_executable_models` remains the one selectable-model read model. Direct
and BYOK Offerings become ready only through a compatible active Workspace
Credential. A Brokered Offering instead becomes ready only when the same
composition advertises `ModelSupplyCapabilityView.cloud_models_enabled`; it
must not require or synthesize a local Credential because the injected
`ModelPublicationResolver` freezes the exact managed route and its existing
egress-gateway credential reference.

```text
Catalog Offering + executor capability
  -> direct/BYOK  -> compatible local Credential -> ready
  -> brokered     -> cloud_models_enabled        -> ready
                    \-> injected publication resolver -> exact managed route
                                                        -> egress gateway custody
```

The capability is a deployment fact, not secret material and not an
authorization grant. If it is false, an otherwise active Brokered Offering is
runtime-unavailable and cannot enter model pickers. If it is true, absence of a
local Credential is expected. Publication still revalidates the exact Cloud
route; an unavailable or unacknowledged route fails there without falling back
to Catalog endpoint data or BYOK. Cloud provider keys never enter the Awaken
Credential repository, process configuration, model directory, or response.
