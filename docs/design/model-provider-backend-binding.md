# Model Provider, Model, And Backend Binding

This document makes model-provider/model/backend binding explicit. Config and
adapter code may select a model provider or backend. Runtime validates the
selected binding and executes through the resolved port; it does not search for a
different model provider inside the execution loop.

## Binding Flow

```text
Config Domain model-provider/model records
  -> ConfigSnapshot
  -> StoredPublication with ExecutableAgentSnapshot
  -> ExecutableAgentRegistrar
  -> Coordinator ExecutableAgentCatalog
  -> ResolvedSpec model-provider/model/backend refs
  -> Session and dispatch freeze the exact snapshot
  -> Worker validates binding and capability profile
  -> ResolvedRun / ResolvedExecutionEnv
  -> LlmExecutor invocation
```

The runtime may reject a binding. It may not silently replace it.

## Spec Graph And Binding Values

Model-provider, model, model-pool, and agent specs are config-domain records.
Binding values are runtime-facing selections derived from those records.

`ModelProviderSpec` is the canonical name for the model-access provider record.

| Concept | Kind | Owner | Meaning | Must not mean |
|---|---|---|---|---|
| `ModelProviderSpec` | config record | Config Domain | one configured model-access provider instance with opaque credential refs and model/backend capability evidence | secret material, authorization grant, model fallback policy, agent runtime family |
| `ModelSpec` | config record | Config Domain | one configured model attached to a provider with model capability metadata | selected runtime binding, provider search, credential choice |
| `ModelPoolSpec` | config record | Config Domain | explicit model selection/fallback policy over model bindings or model refs | hidden runtime provider search, implicit downgrade, authorization |
| `AgentSpec` | config record | Config Domain | behavior assembly that references model selection, tools, plugins, skills, resources, instructions, and requirements | provider credentials, tool implementations, live registry handles, concrete launch or endpoint fields |
| `ModelBinding` | value object | Config compiler / adapter before activation | selected model-provider/model/backend tuple and capability evidence for one run scope | mutable fallback policy or runtime search |
| `BackendProfile` | evidence value | Provider/backend adapter | advertised model-serving features for validation | permission grant or provider selection policy |

The intended relation is:

```text
AgentSpec
  -> model selection ref
     -> ModelSpec
     -> ModelBinding

AgentSpec
  -> model selection ref
     -> ModelPoolSpec
     -> explicit policy selects ModelBinding

ModelBinding
  -> ModelProviderRef
  -> ModelSpec ref
  -> BackendProfile / backend ref
```

`ModelPoolSpec` is the only place where fallback or routing policy belongs. If a
run may fallback from one model to another, the candidate list, order or weights,
health inputs, and downgrade rules must be visible before activation and recorded
in the resolved data needed for replay/debugging.

`ModelProviderSpec` is deliberately narrow. It describes where model calls can go
and what model-facing capability evidence is available. It is not the generic
place to store a future runtime driver, platform adapter, installed command,
cloud bot, gateway endpoint, or discovery result. Those integrations may later
produce model-provider/model/backend evidence consumed by this graph, but their
source and lifecycle records must stay outside `ModelProviderSpec` until they
have their own tested authority boundary.

`AgentSpec` is narrow in the opposite direction. It assembles behavior and
requirements by reference. It may require capabilities that a selected backend
must satisfy, but it does not own concrete execution mechanics such as process
launch commands, local config-home paths, network endpoints, transport auth
headers or capability probe results. Those values belong to
publication inputs, backend registry data, environment/resource adapters, or the
resolved executable snapshot when a concrete slice exists.

## Capability Reconciliation

| Capability | Source of requirement | Source of evidence | Failure rule |
|---|---|---|---|
| streaming | protocol/run options and adapter profile | backend profile | fail before execution if required and unsupported |
| tool calling | visible tool descriptors | model/backend profile | fail before model call if unsupported |
| structured output | resolved output schema | model/model-provider capability | fail before execution or downgrade only when config permits |
| continuation | continuation guard and run policy | backend profile | fail closed when required continuation cannot be represented |
| cancellation | `LiveRunControl` requirement | backend cancellation profile | unsupported cancel is reported before accepting a cancellable run |
| wait/resume | resolved tools in resolved env | model/tool profile | wait not projected unless runtime can resume safely |
| media/modalities | messages, tools, and output schema | model profile | unsupported modality fails before provider call |
| context window | resolved history and compaction plan | model profile | require compaction or reject; no untracked truncation |

Compatibility is not authorization. A compatible backend still needs permission
for protected tools, credentials, and resources.

## Runtime Binding Rules

1. Model-provider/model/backend refs are resolved from published config or
   explicit adapter input accepted as per-run runtime data.
2. Runtime validates the selected refs against catalog fingerprint and backend
   profile.
3. Runtime records the effective binding in run facts or trace metadata needed
   for replay and debugging.
4. A model-provider mismatch fails with a typed capability or binding error.
5. Fallback model providers require explicit config or adapter policy before
   activation; runtime does not invent fallback during execution.

## Model And Backend Binding Role Catalog

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `ModelProviderSpec` | config record | configured model-access provider instance, model API endpoint/config knobs, opaque credential refs, and declared model/backend capability evidence | config store, credential refs, model-provider adapter family | secret material, authorization grant, runtime executor handles, agent runtime identity | model-provider config cannot explain calls or leaks secrets | G3, G9, G22; no-secret serde and binding tests |
| `ModelSpec` | config record | model identity, provider-specific model name, model capability metadata, and limits | `ModelProviderSpec`, model-provider capability evidence | fallback policy, selected runtime binding, credential selection | model config is treated as a selected executable binding | G3, G22; model graph validation tests |
| `ModelPoolSpec` | config record | explicit model selection, routing, fallback, weighting, and downgrade policy | `ModelSpec` refs or `ModelBinding` candidates | hidden provider search, authorization, runtime loop decisions | runtime invents fallback during execution | G22; fallback policy and no-search tests |
| `AgentSpec` | config record | agent behavior assembly and model selection ref | model/model-pool refs, tools, plugins, skills, resource refs, capability requirements | provider credentials, tool implementations, live registry handles, concrete launch or endpoint fields | agent config becomes a god object or hides provider policy | G3, G8, G22; config graph tests |
| `ModelProviderRef` | value object | configured model-provider instance identity | config publication and opaque credential refs | secret material, authorization grant | runtime calls an unconfigured model provider | G3, G22; no-secret serde tests |
| `ModelBinding` | value object | selected model and provider binding | resolved config, adapter overrides when allowed | provider search, credential selection | execution uses an unreviewed model | G22; binding snapshot tests |
| `BackendRequirement` | value object | model/serving features required by a run | tools, output schema, protocol options | provider evidence, permission grant | required feature silently degrades | G22; requirement tests |
| `BackendProfile` | evidence value | advertised model-serving capabilities | provider adapter, model capability evidence | authorization, selection policy | unsupported feature fails late | G22; negotiation tests |
| `BindingValidator` | domain service | reconcile requirement and profile | model binding, model profile, resolved run | fallback selection, public error mapping | wrong model backend accepts the run | G22; fail-closed mismatch tests |
| `LlmExecutor` | execution port | model-provider invocation | validated binding and model request | provider discovery, config writes | runtime execution performs selection | G2, G22; dependency checks |

## First Vertical Slice

1. Validate one `ModelProviderSpec` -> `ModelSpec` -> `AgentSpec` graph and reject a
   missing reference.
2. Resolve one configured model/provider binding into `ResolvedSpec`.
3. Add one `ModelPoolSpec` fallback list and prove selection happens before
   activation.
4. Validate the selected binding against `BackendRequirement` and
   `BackendProfile`.
5. Reject an unsupported tool-calling or streaming requirement before model call.
6. Invoke through `LlmExecutor` only after validation.
7. Record enough binding metadata to explain which model provider/model was used.

## Model Intrinsic Attributes

`ModelSpec` carries the model's own attributes (adopted from goal's
`awaken-agent-contract`), consumed by capability reconciliation and eval cost:

```rust
struct ModelSpec {
    id, provider_id, upstream_model,
    context_window: Option<u32>, max_output_tokens: Option<u32>,
    modalities: Modalities,            // input/output ∈ {Text, Image, Audio, Video, Pdf}
    knowledge_cutoff: Option<String>,  // validated YYYY-MM at the deser boundary
    input_token_price_per_million_usd: Option<f64>,
    output_token_price_per_million_usd: Option<f64>,
}
enum CapabilitySource { ExplicitSpec, ProviderDiscovery, StaticHeuristic } // per-attribute provenance
```

Rules:

- `knowledge_cutoff` is **runtime-trusted** (injected verbatim into the agent's
  system context), so it is validated at the deserialization boundary — closing a
  prompt-injection surface for every source (config, tenant, external registry).
- Each attribute may carry a `CapabilitySource`; only runtime-trusted sources may
  be injected verbatim. `ProviderDiscovery` backfill is deferred (P2).
- Pricing feeds eval `cost_usd` so cost surfaces in regression diffs.

## Inference Routing — a dedicated management plane (`awaken-management-contract`)

Provider access, protocol endpoints, model catalog, and routing are **not** part
of the agent config nor of the credential vault. They live in the **management
plane** (mirroring awaken-next ADR-0088), decomposed by domain into config-store
crates + a resolver service (`awaken-model-catalog` owns provider/endpoint/offering/
model catalog + `InferenceProfile`; `awaken-config-resolver` reads it and resolves;
see [ADR-0043](../adr/0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)
§ decomposition): declarative "what exists, where it runs, who runs it,"
**orthogonal to execution**. Ingress **queries** it to resolve a run; a request never flows
*through* it, and **execution never depends on it** (ADR-0088 **I4** — the same
runtime-unaware boundary as D6/D9 / [ADR-0043](../adr/0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)).
It has its **own config API**, separate from agent config and from credentials.

### Direct and Cloud-brokered sources

The open catalog uses two explicit access paths and never infers one from an
empty credential:

| Path | Local source | Access binding | Authority |
|---|---|---|---|
| Direct/BYOK | manual or Provider API Offering | exact local credential, pool, or explicit no-auth | Awaken catalog, vault, and Profile |
| Awaken Cloud subscription | on-demand `brokered` Offering projection | `brokered` | Cloud identity/entitlement/grant; Awaken Profile selection |

### Deployment capability switches

Identity and model supply are separate deployment axes. `identity_mode` controls
who authenticates users; `cloud_models` controls whether this process may read
the Cloud model projection or obtain brokered inference grants. Both are resolved
once at startup from `~/.awaken/config.toml`, with explicit CLI overrides:

```toml
# "no-login" | "self-managed" | "awaken-cloud"
identity_mode = "no-login"

# "disabled" (default) | "enabled"
cloud_models = "disabled"
```

Equivalent startup overrides are `--identity-mode <mode>` and
`--cloud-models disabled|enabled`. Enabling Cloud models requires
`identity_mode = "awaken-cloud"`; invalid combinations fail startup rather than
silently enabling a network path.

| Identity | `cloud_models` | Login | Local catalog/BYOK | Cloud catalog and brokered inference |
|---|---|---:|---:|---:|
| `no-login` | `disabled` | off | on | off |
| `self-managed` | `disabled` | local IAM | on | off |
| `awaken-cloud` | `disabled` | Cloud | on | off |
| `awaken-cloud` | `enabled` | Cloud | on | on, after authentication/entitlement |
| any non-Cloud identity | `enabled` | — | — | invalid; startup fails closed |

`GET /v1/config/capabilities` is the UI/runtime contract for these axes. When
Cloud models are disabled, brokered rows already persisted locally are projected
as unavailable, profile publication rejects brokered access, materialization
rejects brokered execution, and refresh returns `cloud_models_disabled` without
performing a Cloud inference request. When enabled without an authenticated
Cloud session, refresh returns `cloud_sign_in_required`. There is no implicit
fallback from a brokered candidate to BYOK: fallback remains explicit Profile
policy.

The Models surface therefore presents `Local · BYOK`, `Cloud · sign in required`,
or `Awaken Cloud` from capabilities instead of guessing from catalog contents.
The local catalog and provider API discovery remain available in every valid
mode. Provider model lists are fetched on an explicit user discovery action, not
by a timer; model limits such as context window and maximum output tokens are
optional, provenance-carrying facts, with manual values retained when APIs do not
publish trustworthy values.

Cloud discovery is an authenticated on-demand API projection, not a timer and
not a second writable local catalog. It may publish optional context/output
limits with `brokered` field provenance. Unknown remains absent, stale Cloud
facts may clear only prior `brokered` facts, and manually authored facts win.
Capabilities that vary by protocol remain Offering/endpoint evidence rather
than being flattened into an unsafe model-wide claim.

Billing, subscription, price, quota, usage and charge remain entirely in
`awaken-cloud`. The open product stores only public model coordinates, optional
public attributes, local Profile policy, and opaque grant correlation; it never
stores Cloud internal routes, Provider credentials, prices, balances or usage
ledger facts.

Names follow `awaken-management-contract` (our Agents-product sibling), not raw
oversight-next; the execution side keeps our existing `ResolvedSpec` /
`ModelBinding`. `ModelApiCompat` is the wire/protocol flavor (replaces oversight's
`WireFormat`).

```rust
// awaken-management-contract — catalog domain
enum   Provider { … }                         // vendor
struct ProtocolEndpoint { id, provider_id, flavor: ModelApiCompat, base_url: Option<String> }
struct Offering { model_id, provider_id, flavor: ModelApiCompat }  // a model reachable on a surface
struct ProviderCatalog { … }

// credential axis (see credentials-and-vaults.md)
struct ProviderIdentity { … }                 // binds a credential to a provider (+ per-endpoint enable)
enum   CredentialBinding { … }                // None | Exact | pool member | …

// routing domain
struct InferenceProfile { inference: InferenceConfig, … }   // ← was oversight PresetTier/PresetEntry
struct InferenceTriple { model_id, identity_id, provider_id, flavor }  // ← was InferenceRoutingTriple
fn     resolve_inference(..) -> InferenceResolution         // picks the triple
// model-pool axis: agent def `model_pool: Option<String>` (load-spread + fallback, ADR-0117 D6)
```

For direct Provider authoring, the default `ProtocolEndpoint.id` is derived from
`(provider_id, flavor)` as `<provider_id>.<dialect>`; it is not an additional
client-selected axis. Only multiple endpoints sharing the same dialect require an
`endpoint_name`, producing `<provider_id>.<dialect>.<endpoint_name>`. The retained
id gives immutable publications and internal brokered routes an opaque route pin,
while ordinary Provider setup and selection use the dialect as the protocol-
surface discriminator. Provider descriptors therefore publish dialect/default-
URL pairs and do not maintain a duplicate endpoint suffix.

**The intersection.** `resolve_inference` picks the `ProtocolEndpoint` by
`Offering(model) ∩ flavor` and an eligible `ProviderIdentity` / credential,
yielding one `InferenceTriple = (model × identity × provider × flavor)`.
Model-pool selection and fallback are the `model_pool` axis; the credential is
resolved to a `MaterializedCredential` (already-resolved value) — it never becomes
a secret in any spec, preserving G22.

## How the Managed Agents API consumes management-plane Provider/Model

The public Agent `model.id` remains one string, so official Managed Agents clients
do not need a second Awaken request shape. Provider, endpoint, executor, and model
are encoded only when they are needed to disambiguate execution:

| Model id | Meaning |
|---|---|
| `<model>` | native executor; accepted only when one active Offering matches |
| `<provider>/<model>` | native executor through one Provider |
| `<provider>@<endpoint>/<model>` | native executor through one endpoint selector |
| `acp:<cli>` | ACP CLI with its own default model and Worker-local login |
| `acp:<cli>/<model>` | ACP CLI with an exact backend-owned model |
| `acp:<cli>@<provider>/<model>` | ACP CLI using an Awaken-managed Provider route |
| `acp:<cli>@<provider>@<endpoint>/<model>` | the same with an endpoint selector |
| `a2a:<absolute-http-url>` | one remote A2A Agent; publication discovers and pins its Agent Card security |

The model portion is the complete remainder after the route separator, so ids
such as `anyrouter/qwen/qwen3-235b` mean provider `anyrouter`, model
`qwen/qwen3-235b`. Credentials never appear in this string. Dialect is normally
negotiated from the selected Offering and appears only when it is needed as an
endpoint selector.

The endpoint selector is the dialect for an unnamed/default surface (for example
`glm@anthropic_messages/glm-5`) and the explicit `endpoint_name` when multiple
surfaces share that dialect. If the same short endpoint name occurs under two
dialects, discovery emits `<dialect>.<endpoint_name>` to keep the id unambiguous.
These qualifiers are returned by `/v1/models`; clients do not construct internal
`ProtocolEndpointId` values.

`/v1/models` associates each entry with its Provider through the canonical id.
Different Providers may publish the same model name, so a bare `<model>` is
returned only when globally unique; otherwise discovery returns
`<provider>/<model>`. Multiple endpoints for the same Provider/model add
`@<endpoint>`. The response retains the official `BetaModelInfo` fields rather
than adding a second Provider field that could disagree with the id.

ACP-native Session configuration is orthogonal to route identity. The optional
namespaced extension on the official model object carries only that configuration:

```json
{
  "id": "acp:codex@anyrouter/qwen/qwen3-235b",
  "effort": "high",
  "x_awaken": {
    "acp": {
      "mode": "plan",
      "options": {"reasoning_effort": "high"}
    }
  }
}
```

`id`, `speed`, and `effort` retain their Managed Agents meanings. Omitting
`x_awaken` retains the official shape. An ACP extension on a native or A2A id
fails immediately. Publication checks every mode, option id, and value against
one fresh negotiated Worker profile, then freezes the adapter version,
fingerprint, and exact configuration into the same candidate as the Provider
route.

`parse_managed_model_id` is the sole ACL. It creates a `ModelSelection::Target`
intent rather than an incomplete `Pinned` binding. `select_offering` is the sole
catalog selector. Publication then intersects:

```text
Target
  × one active Offering (Provider + ProtocolEndpoint + dialect)
  × one compatible active Credential
  × Executor capability (native or ACP CLI)
  × current Worker capability when the backend owns execution
  → immutable ResolvedModelCandidate
```

For an ACP Provider route the candidate also contains one
`AcpExecutionProfile`; Provider and BackendOwned provisioning reuse this type.
Runtime Host projects it into the ACP Session, and the handshake checks the
frozen capability fingerprint before a prompt. A2A does not enter the model
planner: the existing remote candidate path validates its URL and same-origin
Agent Card, then freezes transport security and any credential revision.

Zero matches, multiple matches, unsupported dialects, incompatible credential
material/usage, unavailable Worker capabilities, and incomplete endpoint
qualifiers fail before an Agent becomes visible. Runtime receives only the frozen
candidate and never renegotiates Provider, credential, dialect, or executor.

### Developer flow

1. Connect and discover a Provider endpoint. Built-in descriptors provide form
   defaults only; an arbitrary Provider identity is accepted for an installed
   dialect when it supplies `base_url` and a supported authentication method.

   ```http
   POST /v1/config/provider-connections
   Content-Type: application/json

   {
     "idempotency_key": "glm-anthropic-primary",
     "workspace_id": "workspace-a",
     "provider_id": "glm",
     "display_name": "GLM",
     "dialect": "anthropic_messages",
     "base_url": "https://example.invalid/anthropic/v1",
     "secret": "..."
   }
   ```

   The command tests authentication and discovers models before visibility. It
   stages a newly created credential as disabled, records the Provider, derived
   endpoint id, and Offerings, then activates that credential. A catalog-write
   failure therefore cannot expose an unverified credential/model route. A
   second endpoint using the same dialect supplies `endpoint_name`. A credential
   created by this command records that exact canonical endpoint as its immutable
   proof scope; it cannot make a sibling endpoint executable. A separately
   entered provider-wide credential has no endpoint restriction and may be bound
   explicitly where the operator intends it.

   Provider and dialect are separate axes. GLM may have an
   `anthropic_messages` endpoint and an `open_ai_chat` endpoint under the same
   `provider_id`; Qwen or AnyRouter may expose slash-bearing model ids without
   changing the grammar. Credential kind and delivery usage are validated
   independently from endpoint dialect.

2. List the executable, Workspace-scoped Managed ids:

   ```http
   GET /v1/models
   ```

   The response is derived from Catalog × Credential × the installed executor
   capability matrix; it is not a static vendor list. Bare ids are returned only
   when unique; provider and endpoint qualifiers are added only when required.
   Agent publication revalidates the same matrix and, for ACP, requires fresh
   negotiated Worker evidence before visibility.

3. Create and publish an Agent using the ordinary Managed Agents field:

   ```http
   POST /v1/agents
   Content-Type: application/json

   {"name":"support","model":"acp:claude@glm/glm-5","tools":[]}
   ```

   Adapter-native configuration stays in the same model field:

   ```json
   {
     "name": "research",
     "model": {
       "id": "acp:codex@anyrouter/qwen/qwen3-235b",
       "x_awaken": {
         "acp": {
           "mode": "plan",
           "options": {"reasoning_effort": "high"}
         }
       }
     },
     "tools": []
   }
   ```

   A backend-owned CLI login uses `acp:codex` for its default model or
   `acp:codex/gpt-5` for an exact model. A remote Agent uses
   `a2a:https://agent.example/a2a`; it is absent from `/v1/models` because it is
   an Agent, not a model. Backend-owned ACP routes are runtime capabilities, so
   clients discover their availability through `/v1/config/capabilities`;
   `/v1/models` owns Provider-backed model routes only.

   Create/update dry-runs the complete publication first and returns `400` for an
   unsupported combination. The config authoring API may retain drafts; the
   Managed Agents API exposes only successfully published Agents.

4. Create a Session with the Agent id through `/v1/sessions`. The Session inherits
   the complete immutable publication. `metadata.awaken.model` is inert metadata,
   not an execution path. An official per-Session model override is accepted only
   when it names the Agent's already-published model; selecting another route
   requires creating or updating an Agent so model and backend pins cannot diverge.
   The Session baseline keeps the public Managed id for API projection and a
   separately fingerprinted execution `model_ref` from the publication. Runtime
   consumes only the latter and never reparses Provider/endpoint/ACP syntax.

This is the complete public flow: Provider connection and model discovery,
Agent create/update, and Agent use require no internal catalog endpoint id,
credential id, runtime id, or separate ACP execution API.

## Guardrails

G3, G9, G22, and G26 in [INVARIANTS](../INVARIANTS.md).
