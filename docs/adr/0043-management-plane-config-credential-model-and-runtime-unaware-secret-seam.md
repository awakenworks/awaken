# ADR-0043: Management/Runtime Boundary and Resolved-Secret Runtime Contract

- Status: Proposed
- Date: 2026-07-04
- Amended 2026-07-04: provider/endpoint/offering/routing live in a **dedicated
  management-plane crate `awaken-management-contract`** (mirroring awaken-next
  ADR-0088), orthogonal to execution (execution never depends on it — I4 = D6/D9);
  names align to that crate (`ProtocolEndpoint`/`Offering`/`InferenceProfile`/
  `InferenceTriple`/`ModelApiCompat`/`model_ref::ResolvedModel`), not raw
  oversight-next; the Managed API's `model` resolves via model-extension
  (`metadata.awaken` model axis → `reconcile_model_ref` → `resolve_inference`).
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
(`ProtocolEndpoint`, `Offering`, `ProviderCatalog`, `ProviderIdentity`,
`CredentialBinding`, `InferenceProfile`, `InferenceTriple`, `model_ref`) reusing
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

## Decision

### 1. The management plane owns the model; the runtime consumes a compiled snapshot

Management-plane crates author, version, tenant, and **resolve**:
`awaken-agent-contract` (specs), **`awaken-management-contract`** (provider
catalog / endpoints / offerings / inference routing — its own config API, ADR-0088,
orthogonal to execution, **execution never depends on it, I4**), `awaken-credential-vault`
(vault/credential), `awaken-managed-bridge` (Managed wire ACL). The runtime links
**none** of them. It receives a compiled `ExecutableAgentSnapshot`/`RunnableConfig`
in which every credential is an **already-resolved value** (a `RedactedString`), or
absent (host-native / egress proxy). Dependency direction is management plane →
`awaken-runtime-contract`, never reverse.

The **Managed Agents API consumes the management plane by reference, not by
value**: the public `model` string + a `metadata.awaken` model axis is decoded
(`model_ref`) and resolved (`reconcile_model_ref` → `resolve_inference` →
`InferenceTriple`), admitted against the model-directory capability (ADR-0091 D5),
fail-closed — see [model-provider-backend-binding](../design/model-provider-backend-binding.md).

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
  an `InferenceProfile` entry's `CredentialBinding` (`Exact` or pool) →
  `ProviderIdentity` → `CredentialSource` → `SecretStore` (see
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

### 3. "Normal config" and "vault config" — resolved before the runtime

Both reach the runtime as a fully-resolved snapshot; the runtime never resolves:

- **Normal config** → the credential is already a `RedactedString` (or a resolved
  env value); the runtime runs standalone (goal's shape today; the P0 path).
- **Vault config** → the **host** resolves the `CredentialBinding` / handle via
  `awaken-credential-vault` first, then hands the runtime the resolved snapshot.

"接收 vault" means the host accepts a vault-backed config and resolves it upstream —
the runtime sees only the resolved value, identical to the normal-config case.

### 4. Hard constraints

- **No inline-secret-in-config** — a secret always lives in a `CredentialSource`
  (vault or host-native), never embedded in a spec (oversight-consistent); the
  secret-free invariant holds on every profile.
- **A missing handle fails closed** — no ambient env fallback.
- **`credential.*` authz is required** (ADR-0042); the draft credential domain has
  none by itself.
- Encryption-at-rest and secretless egress-proxy delivery are `SecretStore`/
  `SecretResolver` **adapters** in the host/credential layer, never runtime or
  domain concerns (see the design docs).

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
(`awaken-server-local` assembly); **credential is the first split candidate**
(blast-radius / `secretd`).

### Store vs resolver (DDD service vs aggregate)

Config stores own aggregates; the resolver owns **none** — it reads the stores and
emits an `ExecutableAgentSnapshot`. The earlier "inference domain" was really a
resolver mislabeled; `InferenceProfile` (routing policy) is an aggregate and moves
into `awaken-model-catalog`.

### Intent-revealing crate names

| Purpose | Crate | Kind |
|---|---|---|
| provider/endpoint/offering/model catalog + `InferenceProfile` routing policy | `awaken-model-catalog` | config store |
| vault/credential/binding/pool/identity + `SecretStore` | `awaken-credential-vault` | config store |
| agent config authoring/publish/compile | `awaken-agent-config` *(was `awaken-config-store`)* | config store |
| read all config → `ExecutableAgentSnapshot` (`InferenceTriple` + `MaterializedCredential`) | `awaken-config-resolver` *(was the misnamed `awaken-inference`)* | resolver **service** — no aggregate, no execution |
| run the model | `awaken-provider-genai` | execution |
| Managed Agents wire | `awaken-protocol-managed` | front door |
| Managed wire ⇄ domain ACL | `awaken-managed-bridge` | ACL |
| admin config API (provider/endpoint/inference-profile) | `awaken-admin-config-api` | API assembly |
| the one service | `awaken-server-local` | assembly |

Rule: **name = responsibility, not layer.** "inference" is reserved for where
inference actually runs (`awaken-provider-genai`); "resolver" for config→executable;
"store" for persistence; "catalog" for the model registry.

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
contract is the **official `@anthropic-ai/sdk`**; `awaken-protocol-managed` +
`awaken-managed-bridge` conform to it (never regenerate Anthropic's SDK).

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
