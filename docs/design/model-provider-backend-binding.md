# Model Provider, Model, And Backend Binding

This document makes model-provider/model/backend binding explicit. Config and
adapter code may select a model provider or backend. Runtime validates the
selected binding and executes through the resolved port; it does not search for a
different model provider inside the execution loop.

## Binding Flow

```text
Config Domain model-provider/model records
  -> ConfigSnapshot
  -> RegistryPublication
  -> RuntimeCatalogInstall
  -> RuntimeCatalogInstaller
  -> ResolvedSpec model-provider/model/backend refs
  -> RunResolver validates binding and capability profile
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

Names follow `awaken-management-contract` (our Agents-product sibling), not raw
oversight-next; the execution side keeps our existing `ResolvedSpec` /
`ModelBinding`. `ModelApiCompat` is the wire/protocol flavor (replaces oversight's
`WireFormat`).

```rust
// awaken-management-contract — catalog domain
enum   Provider { … }                         // vendor
struct ProtocolEndpoint { provider_id, flavor: ModelApiCompat, base_url: Option<String> }
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

**The intersection.** `resolve_inference` picks the `ProtocolEndpoint` by
`Offering(model) ∩ flavor` and an eligible `ProviderIdentity` / credential,
yielding one `InferenceTriple = (model × identity × provider × flavor)`.
Model-pool selection and fallback are the `model_pool` axis; the credential is
resolved to a `MaterializedCredential` (already-resolved value) — it never becomes
a secret in any spec, preserving G22.

## How the Managed Agents API consumes management-plane Provider/Model

The public Managed Agents `model` field stays **Anthropic-compatible** — a bare
model string. The binding detail rides in `metadata.awaken` and is decoded by a
**model-axis codec** (`agent_model_codec` / `ModelAxis`) into a management-plane
model reference. This is the **"model 扩展解析" (model-extension resolution)** — the
Managed API never inlines provider/endpoint/credential config; it references a
model, and the management plane resolves it.

At resolve time ingress **queries** the management plane:

1. `reconcile_model_ref(..)` → `ResolvedModel { model_id, flavor: ModelApiCompat }`
   (or `ModelRefBinding::BuiltinDefault` for an ACP adapter's built-in backend).
   Fail-closed if a `Native` def's required model ref does not resolve to a
   `ModelSpec` (fail-closed).
2. `resolve_inference(..)` → an `InferenceTriple` from the catalog +
   `ProviderIdentity` + `CredentialBinding` (+ `model_pool` selection).
3. The selected model is admitted against the **resolved model-directory
   capability** (ADR-0091 D5) — fail-closed.
4. The triple + a `MaterializedCredential` feed execution; execution never touches
   `awaken-management-contract` (I4).

Agent-def kinds set whether a model ref is required: **Native** (in-proc brain)
**requires** a `ModelSpec` ref; **AcpLaunched** has an optional capability-gated
backend ref (else built-in default); **Remote** (A2A / Coze) owns no local model.

**P0 implementation subset.** Model the full graph in `awaken-management-contract`,
but P0 wires only the single-`ProviderIdentity` + single-`ProtocolEndpoint` +
`CredentialBinding::Exact` path — one Provider, one endpoint, one credential per
run. Model pool, multi-candidate `InferenceProfile` failover, and endpoint pinning
are P1. `ProviderDiscovery` model-capability backfill is P2.

## Guardrails

G3, G9, G22, and G26 in [INVARIANTS](../INVARIANTS.md).
