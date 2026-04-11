# Provider and Model Configuration

Awaken keeps provider wiring and model selection separate. The runtime always resolves an agent through this chain:

```text
AgentSpec.model_id
  -> ModelRegistry[model id]
  -> ModelBinding { provider_id, upstream_model }
  -> ProviderRegistry[provider id]
  -> Arc<dyn LlmExecutor>
  -> InferenceRequest.upstream_model = upstream_model
```

## Terms

| Term | Type | Meaning |
|---|---|---|
| Agent model id | `AgentSpec.model_id` | Stable model registry id used by an agent, for example `"default"` or `"research"`. |
| Runtime model binding | `ModelBinding` | Runtime mapping from model id to provider id and upstream model name. |
| Config model binding | `ModelBindingSpec` | Serializable mapping stored in managed config. It is compiled into `ModelBinding`. |
| Provider config | `ProviderSpec` | Serializable provider settings used by the server to construct an executor. |
| Provider executor | `Arc<dyn LlmExecutor>` | Live provider client used to execute inference. |
| Upstream model name | `ModelBinding.upstream_model`, `ModelBindingSpec.upstream_model`, `InferenceRequest.upstream_model` | The actual model string sent to the provider API. |

The important distinction is:

- `AgentSpec.model_id` is a registry id.
- `ModelBindingSpec.upstream_model`, `ModelBinding.upstream_model`, and `InferenceRequest.upstream_model` are upstream provider model names.

## Programmatic builder path

Use this path when the application owns provider construction in code.

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::{AgentRuntimeBuilder, AgentSpec};

let agent = AgentSpec::new("assistant")
    .with_model_id("default")
    .with_system_prompt("You are helpful.");

let runtime = AgentRuntimeBuilder::new()
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model_binding("default", ModelBinding {
        provider_id: "openai".into(),
        upstream_model: "gpt-4o-mini".into(),
    })
    .with_agent_spec(agent)
    .build()?;
```

`build()` validates every registered agent by resolving its model id and provider id. Missing models, providers, or plugins fail at startup.

## Managed config path

Use this path when the server owns dynamic config through `ConfigStore`.

Managed config is stored by namespace:

| Namespace | Serializable type |
|---|---|
| `providers` | `ProviderSpec` |
| `models` | `ModelBindingSpec` |
| `agents` | `AgentSpec` |
| `mcp-servers` | `McpServerSpec` |

Example config documents:

```json
{
  "id": "openai",
  "adapter": "openai",
  "api_key": "sk-...",
  "base_url": null,
  "timeout_secs": 300
}
```

```json
{
  "id": "default",
  "provider_id": "openai",
  "upstream_model": "gpt-4o-mini"
}
```

```json
{
  "id": "assistant",
  "model_id": "default",
  "system_prompt": "You are helpful."
}
```

The server compiles these documents into runtime registries:

```text
ProviderSpec -> ProviderExecutorFactory -> Arc<dyn LlmExecutor>
ModelBindingSpec    -> ModelBinding
AgentSpec    -> AgentSpecRegistry
```

Configuration documents use only canonical field names. Use `model_id` on
agents, `provider_id` and `upstream_model` on model bindings, and
`fallback_upstream_models` in retry or inference overrides.

The candidate registry set is validated before it replaces the active runtime snapshot. If validation fails, the config write is rolled back.

## Migration from legacy model fields

This version intentionally rejects legacy provider/model field names instead of
silently normalizing them. Update stored config, test fixtures, and clients
before upgrading:

| Old field or shape | New canonical form |
|---|---|
| `AgentSpec.model` | `AgentSpec.model_id` |
| `ModelBindingSpec.provider` | `ModelBindingSpec.provider_id` |
| `ModelBindingSpec.model` | `ModelBindingSpec.upstream_model` |
| `InferenceOverride.model` | `InferenceOverride.upstream_model` |
| `fallback_models` | `fallback_upstream_models` |
| `AgentSystemConfig.models` as an object keyed by model id | `AgentSystemConfig.models` as a list of `ModelBindingSpec` objects with explicit `id` |

Upgrade check:

```bash
rg '"model"\s*:|"provider"\s*:|fallback_models' config/ docs/ tests/
```

Each match should be checked. Protocol payloads may still use a field named
`model` when they mirror an external protocol; managed Awaken config should not.

## Provider secrets

Provider API keys are write-only through the config API:

- responses redact `api_key`;
- responses expose `has_api_key: true` when a key is stored;
- updating a provider without `api_key` preserves the existing key;
- setting `api_key` to `null` or an empty string clears it.

## Runtime snapshot behavior

The runtime does not read `ConfigStore` during each inference step. Managed config changes are compiled into a new registry snapshot:

```text
ConfigStore change -> compile RegistrySet -> validate -> replace runtime snapshot
```

New runs use the latest published snapshot. Active runs keep the snapshot they started with.

## Inference overrides

`InferenceOverride.upstream_model` and `InferenceOverride.fallback_upstream_models` use upstream model names for the already resolved provider. They do not re-resolve `AgentSpec.model_id` and do not switch provider executors.

At execution time the primary override is applied to `InferenceRequest.upstream_model`; executors should treat that field as the single source of truth for the primary upstream model. Remaining override fields carry generation parameters and fallback upstream models.

Use model overrides for same-provider model changes:

```rust,ignore
use awaken::contract::inference::InferenceOverride;

let overrides = InferenceOverride {
    upstream_model: Some("gpt-4o".into()),
    fallback_upstream_models: Some(vec!["gpt-4o-mini".into()]),
    ..Default::default()
};
```

Use a different `AgentSpec.model_id` or agent handoff when execution must move to another provider.

## Retry and fallback

Per-agent retry is read through the `"retry"` section via `RetryConfigKey`. When the section is absent, `LlmRetryPolicy::default()` is used. Resolution wraps the provider executor in `RetryingExecutor` when the resulting policy has retries or fallback upstream models. Set `max_retries` to `0` and leave `fallback_upstream_models` empty to disable the wrapper.

Provider factories return provider executors; retry is added by the resolve pipeline, not hidden inside provider construction.

For collected execution, retry and fallback apply to the full inference call. For streaming execution, retry and fallback apply while opening the stream. Once a stream has started, later stream-item errors are surfaced directly because retrying would duplicate already emitted deltas.
