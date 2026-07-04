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
| Credential Domain / Product | vault schema, credential CRUD, OAuth refresh, account grouping, availability projection, operator UX |
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

## Concrete Model (aligned to oversight-next)

This model mirrors oversight-next exactly. Two **orthogonal** axes — do not
conflate them:

- **Materialization** — *where* a secret physically lives: `CredentialSource.kind`.
- **Selection** — *which* source a run uses: `CredentialBinding` (+ `CredentialPool`).

There is **no inline-secret-in-config path** (oversight-consistent): a secret
always lives in a `CredentialSource` (vault or host-native), never embedded in a
spec. The runtime sees neither axis — both resolve management-side into a
`SecretInput` (see [ADR-0043](../adr/0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)).

### Credential source — the stored row (secret-free)

```rust
struct CredentialSource {                     // supersedes this page's earlier `CredentialRecord`
    id, workspace_id,
    kind: CredentialKind,                     // Vault | HostNative | WorkerLocal | EnvPassthrough | ExternalRef
    provider_id: Option<String>, adapter_registration_id: Option<String>,
    auth: CredentialAuth,                     // neutral; ACL maps the managed wire tags
    material_ref: Option<SecretRef>,          // → SecretStore; None for host-native (secret never crosses control plane)
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

### Provider identity — binds a credential to a provider + per-endpoint toggle

```rust
struct ProviderIdentity {
    id, workspace_id, provider_id: Option<String>,
    credential_binding: Option<CredentialBinding>,     // vault-backed, never inline
    max_concurrency, refresh_model,                    // none | static | rotating
    disabled_endpoint_ids: Vec<ProtocolEndpointId>,    // toggle (credential × interface) off
    version,
}
```

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

The ACL (`awaken-managed-bridge`) maps this wire ⇄ the neutral domain below; the
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
| At rest | `SecretStore` sealed-AEAD, key separate; or don't store (`Env`/host-native) | stolen disk / DB dump / backup |
| In use | materialize at injection seam only; `Networking.allowed_hosts`; sandbox can't see vault | compromised/malicious agent exfiltration |
| Access control | `workspace_id` scoping + `credential.*` authz (ADR-0042) | wrong-tenant read/use |
| Blast radius | optional `secretd`/egress proxy; process-group reap | master-key exposure; residual-process leak |

Must-enforce: no inline-secret-in-config (a secret always lives in a
`CredentialSource`, oversight-consistent); AEAD keys physically separate from
ciphertext; `credential.*` authz is required.

## Egress-Proxy Delivery (Managed, Untrusted Agents)

For untrusted third-party agent code, point `ProtocolEndpoint.base_url` at a
credential-injecting egress proxy that holds the credential and enforces
`allowed_hosts`; the agent becomes **secretless**. This is another
`SecretResolver`/delivery adapter — the runtime seam is unchanged. It is strictly
more secure for network credentials **but only if egress is network-locked to the
proxy** (sandbox netns/firewall); it composes with, and does not replace, network
isolation.

## Staging

Model the **full oversight-next entity graph** from the start (so no rework), but
wire only a subset in P0:

- **P0 (single-machine):** `CredentialSource` (kind `Vault`/`EnvPassthrough`) +
  `SecretStore` (inmem/plaintext-file) + `CredentialBinding::Exact` only. No
  pool, no multi-tier routing, no `ProviderIdentity` per-endpoint toggle.
- **P1 (managed):** `CredentialPool` + `CredentialBinding::OneOfCredentialPool` +
  `CredentialSelectionPolicy`; `ProviderIdentity.disabled_endpoint_ids`;
  sealed-AEAD `SecretStore`; Managed ACL; `credential.*` authz + workspace scope.
- **P2:** `OAuth` refresh loop, `Networking.allowed_hosts` / `injection_location`
  enforcement, `CredentialValidation` audit, egress-proxy secretless delivery.

## Guardrails

G8 and G9 in [INVARIANTS](../INVARIANTS.md).
