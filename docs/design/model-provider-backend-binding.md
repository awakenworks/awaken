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

## Guardrails

G3, G9, G22, and G26 in [INVARIANTS](../INVARIANTS.md).
