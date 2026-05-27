---
title: "Provider and Model Configuration"
description: "Awaken keeps provider wiring and model selection separate. Local agent execution resolves provider and model selection through this chain:"
---

Awaken keeps provider wiring and model selection separate. Local agent execution resolves provider and model selection through this chain:

```text
AgentSpec.model_id
  -> ModelRegistry[model id]
  -> ModelSpec { provider_id, upstream_model, capabilities, pricing }
  -> ProviderRegistry[provider id]
  -> Arc<dyn LlmExecutor>
  -> InferenceRequest.upstream_model = upstream_model
```

Endpoint-backed agents skip this local provider/model chain. They are resolved as non-local `ResolvedExecution` values and executed by the configured `ExecutionBackend`.

## Terms

| Term | Type | Meaning |
|---|---|---|
| Agent model id | `AgentSpec.model_id` | Stable model registry id used by an agent, for example `"default"` or `"research"`. |
| Model spec | `ModelSpec` | Unified serializable + runtime type. Carries addressing (`id`, `provider_id`, `upstream_model`), intrinsic capabilities (`context_window`, `max_output_tokens`, `modalities`, `knowledge_cutoff`), and pricing. Stored in managed config and returned by `ModelRegistry::get_model`. |
| Provider config | `ProviderSpec` | Serializable provider settings used by the server to construct an executor. |
| Provider executor | `Arc<dyn LlmExecutor>` | Live provider client used to execute inference. |
| Upstream model name | `ModelSpec.upstream_model`, `InferenceRequest.upstream_model` | The actual model string sent to the provider API. |

The important distinction is:

- `AgentSpec.model_id` is a registry id.
- `ModelSpec.upstream_model` and `InferenceRequest.upstream_model` are upstream provider model names.

## Programmatic builder path

Use this path when the application owns provider construction in code.

```rust
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::ModelSpec;
use awaken::{AgentRuntimeBuilder, AgentSpec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = AgentSpec::new("assistant")
        .with_model_id("default")
        .with_system_prompt("You are helpful.");

    let runtime = AgentRuntimeBuilder::new()
        .with_provider("openai", Arc::new(GenaiExecutor::new()))
        .with_model(ModelSpec::new("default", "openai", "gpt-4o-mini"))
        .with_agent_spec(agent)
        .build()?;

    let _runtime = runtime;
    Ok(())
}
```

`build()` validates every registered agent by resolving its model id and provider id. Missing models, providers, or plugins fail at startup.

For tests and local development, `MockProviderProfile` gives explicit mock
provider wiring without global environment switches:

```rust
use awaken::{AgentRuntimeBuilder, MockProviderProfile};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = AgentRuntimeBuilder::new()
        .with_mock_provider_profile(MockProviderProfile::new("mock", "mock-model"))
        .build()?;

    let _runtime = runtime;
    Ok(())
}
```

## Managed config path

Use this path when the server owns dynamic config through `ConfigStore`.

Managed config is stored by namespace:

| Namespace | Serializable type |
|---|---|
| `providers` | `ProviderSpec` |
| `models` | `ModelSpec` |
| `agents` | `AgentSpec` |
| `mcp-servers` | `McpServerSpec` |

Example config documents:

```json
{
  "id": "openai",
  "adapter": "openai",
  "api_key": "sk-...",
  "base_url": null,
  "timeout_secs": 300,
  "adapter_options": {}
}
```

```json
{
  "id": "default",
  "provider_id": "openai",
  "upstream_model": "gpt-4o-mini",
  "context_window": 128000,
  "max_output_tokens": 16384,
  "modalities": { "input": ["text", "image"], "output": ["text"] },
  "knowledge_cutoff": "2024-10",
  "input_token_price_per_million_usd": 0.15,
  "output_token_price_per_million_usd": 0.60
}
```

```json
{
  "id": "assistant",
  "model_id": "default",
  "system_prompt": "You are helpful."
}
```

### ProviderSpec fields

| Field | Type | Default | Description |
|---|---|---|---|
| `id` | `String` | required | Provider identifier referenced by `ModelSpec.provider_id` |
| `adapter` | `String` | required | GenAI adapter kind (e.g. `"openai"`, `"anthropic"`, `"ollama"`) |
| `api_key` | `Option<RedactedString>` | `None` | Wrapped in `RedactedString`; redacted in `Debug`/`Display`. Wire format is a plain JSON string. Empty-string input deserializes as `None` so a stored key is preserved when the field is omitted on update |
| `base_url` | `Option<String>` | `None` | Override base URL for proxies or self-hosted deployments. Empty-string input deserializes as `None` |
| `timeout_secs` | `u64` | `300` | Request timeout in seconds |
| `adapter_options` | `BTreeMap<String, Value>` | `{}` | Adapter-specific non-secret options. The OpenAI-compatible adapter recognizes `headers` (an object of string→string pairs added as default request headers). `model_discovery_schema` (`"openai"`/`"openai-compatible"` or `"gemini"`) opts a custom adapter into `/models` capability discovery using that schema (see Model capability sources). Unknown keys are accepted by the schema and ignored at build time. Secrets must use `api_key`; do not store credentials here |

`ProviderSpec` deserialization ignores unknown top-level fields for stored-config
compatibility. Config write and validate surfaces call `validate_provider_spec`
and reject unknown fields so new records cannot persist silently ignored
settings. Use `validate_model_spec` for the same canonical validation on
model specs, and `validate_unique_model_ids` when validating a `Vec<ModelSpec>`
collection before persisting.

Example with custom headers:

```json
{
  "id": "bigmodel",
  "adapter": "openai",
  "api_key": "<redacted>",
  "base_url": "https://open.bigmodel.cn/api/paas/v4",
  "adapter_options": {
    "headers": {
      "X-Tenant-Id": "team-42"
    }
  }
}
```

The server compiles these documents into runtime registries:

```text
ProviderSpec -> ProviderExecutorFactory -> Arc<dyn LlmExecutor>
ModelSpec    -> ModelRegistry (stored as-is; no spec/runtime split)
AgentSpec    -> AgentSpecRegistry
```

Configuration documents use only canonical field names. Use `model_id` on
agents, and `provider_id` plus `upstream_model` on model specs.

The candidate registry set is validated before it replaces the active runtime snapshot. If validation fails, the config write is rolled back.

### Model capability sources

Capability fields follow this priority during resolution:

1. Explicit fields stored in `ModelSpec`.
2. Provider model metadata discovered from `/models` during registry publish.
3. Built-in static heuristics for common model families.

Static heuristics are conservative metadata only. Runtime input-modality
enforcement and automatic knowledge-cutoff context are enabled only when the
field came from explicit `ModelSpec` config or provider discovery.

Provider discovery coverage is adapter-specific. Only adapters with a known
`/models` schema are probed: `openai`/`openrouter` (OpenAI-compatible) and
`gemini`/`google` (Gemini). An unknown or custom adapter is never silently
treated as OpenAI-compatible; to discover a custom OpenAI-/Gemini-compatible
gateway, opt in with `adapter_options.model_discovery_schema`. Gemini/Google
discovery currently backfills token limits only; configure `modalities` and
`knowledge_cutoff` explicitly when those fields should drive runtime guards or
context injection. Vertex models may still receive static heuristic metadata,
but Vertex provider discovery is not enabled unless a future adapter supplies a
complete discovery URL/auth implementation.

Explicit `ModelSpec.knowledge_cutoff` is validated when the spec is
deserialized: it must be an ISO `YYYY-MM` or `YYYY-MM-DD` date, so a malformed
or injected value is rejected before it can reach the resolved model or the
knowledge-cutoff system context.

When `ModelSpec.modalities.input` is explicit or provider-discovered and
non-empty, unsupported request content blocks are rejected before the provider
call. Text blocks in system, user, assistant, and tool-result content do not
consume a `text` modality; `modalities.input` gates only media the model must
read (`image`, `audio`, `video`, and documents identifiable as `pdf`). Tool
*calls* and reasoning/thinking blocks are protocol structures, not model input
modalities, so they are not modality-gated. Media embedded inside a
`ToolResult.content` *is* validated against `modalities.input`, because the
model still has to read that media: the guard recurses into tool results and
checks each contained media block. When
`knowledge_cutoff` is explicit or provider-discovered, the resolver installs
`knowledge_cutoff_context`, which injects one system context message per
inference boundary. Disable it per agent with:

```json
{
  "sections": {
    "knowledge_cutoff_context": { "enabled": false }
  }
}
```

Provider discovery is a full snapshot for a provider definition. If a later
publish cannot refresh `/models`, the last successful snapshot for the same
provider signature is kept; changing the provider endpoint/options invalidates
that cached snapshot.

Model pool members use the same capability resolution and modality guard as
single models. Pool-level knowledge-cutoff context is installed only when every
member exposes the same trusted cutoff.

## Migration from legacy model fields

This version intentionally rejects legacy provider/model field names instead of
silently normalizing them. Update stored config, test fixtures, and clients
before upgrading:

| Old field or shape | New canonical form |
|---|---|
| `AgentSpec.model` | `AgentSpec.model_id` |
| `ModelBindingSpec` type | `ModelSpec` (unified; carries capabilities + pricing) |
| `ModelBindingSpec.provider` | `ModelSpec.provider_id` |
| `ModelBindingSpec.model` | `ModelSpec.upstream_model` |
| Runtime `ModelBinding` (provider_id + upstream_model only) | `ModelSpec` (returned in full by `ModelRegistry::get_model`) |
| `with_model_binding(id, binding)` builder | `with_model(spec)` (id from `spec.id`) |
| `validate_model_binding_spec` | `validate_model_spec` |
| `ProviderRemovalPolicy::CascadeUnusedModelBindings` | `CascadeUnusedModels` |
| Rust-internal field `model_bindings: Vec<…>` | `models: Vec<ModelSpec>` (wire JSON key was already `models`) |
| `InferenceOverride.model` | `InferenceOverride.upstream_model` |
| `AgentSystemConfig.models` as an object keyed by model id | `AgentSystemConfig.models` as a list of `ModelSpec` objects with explicit `id` |

Upgrade check:

```bash
rg '"model"\s*:|"provider"\s*:' config/ docs/ tests/
```

Each match should be checked. Protocol payloads may still use a field named
`model` when they mirror an external protocol; managed Awaken config should not.

## Provider secrets via the config API

The config API treats `api_key` as write-only:

- list/get responses replace `api_key` with `has_api_key: true|false`;
- `PUT` with `api_key` omitted keeps the stored key;
- `PUT` with `api_key: null` or `api_key: ""` clears it.

The in-memory representation is `RedactedString` (see
[config reference — secret handling](/awaken/reference/config/#secret-handling)).

## Runtime snapshot behavior

The runtime does not read `ConfigStore` during each inference step. Managed config changes are compiled into a new registry snapshot:

```text
ConfigStore change -> compile RegistrySet -> validate -> replace runtime snapshot
```

New runs use the latest published snapshot. Active runs keep the snapshot they started with.

## Runtime Registry Updates

`RegistryHandle` exposes provider update operations for applications that
manage provider executors programmatically:

These operations update only the current in-memory runtime snapshot. They do
not write to `ConfigStore`; the next managed config publish can replace the
snapshot with the state compiled from `ConfigStore`.

| Method | Behavior |
|---|---|
| `register_provider(id, executor)` | Add a new provider and publish a validated snapshot |
| `replace_provider(id, executor)` | Replace an existing provider executor without rebuilding unrelated registries |
| `preview_remove_provider(id)` | Return dependent model and agent ids without mutating the snapshot |
| `remove_provider(id, policy)` | Remove a provider after checking dependent models and agents |

`ProviderRemovalPolicy::BlockIfReferenced` rejects removal while any model
references the provider. `ProviderRemovalPolicy::CascadeUnusedModels`
also removes models that reference the provider, but only when no agent
uses them. `ProviderRemovalPreview` reports the provider id,
referencing `model_ids`, affected `agent_ids`, and whether each policy is
currently allowed. On success, `ProviderRemovalImpact` reports the provider id
and removed model ids; on dependency conflicts,
`RegistryUpdateError::ProviderInUse` includes the referenced model and agent ids.

Use `rebuild_agent_model_provider_registries(base, update)` when a config source
has produced a full replacement set for agents, models, and providers. It
preserves tools, plugins, and execution backends from the base registry set,
then validates the candidate before returning it.

Diagnostics are available without publishing a snapshot:

| Function | Reports |
|---|---|
| `diagnose_registry_set(registries)` | Missing models, providers, plugins, and delegate agents |
| `diagnose_registry_set_serializable(registries)` | Same diagnostics as stable payloads with `code`, `severity`, `resource`, optional `depends_on`, and `message` |
| `validate_registry_set(registries)` | Same checks as an error result |
| `diagnose_agent_spec(registries, spec)` | Problems for one agent against an existing registry set |
| `validate_agent_spec(registries, spec)` | Same agent checks as an error result |

## Inference overrides

`InferenceOverride.upstream_model` uses an upstream model name for the already resolved provider. It does not re-resolve `AgentSpec.model_id`, does not switch provider executors, and is rejected for model-pool-backed agents because the pool chooses its member internally.

At execution time the override is applied to `InferenceRequest.upstream_model`; executors should treat that field as the single source of truth for the upstream model. Remaining override fields carry generation parameters.

Use model overrides for same-provider model changes:

```rust
use awaken::contract::inference::InferenceOverride;

let overrides = InferenceOverride {
    upstream_model: Some("gpt-4o".into()),
    ..Default::default()
};
```

Use a `ModelPoolSpec`, a different `AgentSpec.model_id`, or agent handoff when execution must move to another model/provider.

## Retry and model pools

Per-agent retry is read through the `"retry"` section via `RetryConfigKey`. When the section is absent, `LlmRetryPolicy::default()` is used. Resolution wraps the provider executor in `RetryingExecutor` when the resulting policy has retries. Set `max_retries` to `0` to disable the wrapper.

Provider factories return provider executors; retry is added by the resolve pipeline, not hidden inside provider construction.

For collected execution, retry applies to the full inference call. For streaming execution, retry applies while opening the stream. Once a stream has started, later stream-item errors are surfaced directly because retrying would duplicate already emitted deltas. Model failover belongs in `ModelPoolSpec`.

## Related

- [Configure Agent Behavior](/awaken/how-to/configure-agent-behavior/)
- [Config](/awaken/reference/config/)
- [Agent Resolution](/awaken/explanation/agent-resolution/)
