---
title: "Provider and Model Configuration"
description: "Awaken keeps provider wiring and model selection separate. Local agent execution resolves provider and model selection through this chain:"
---

Awaken keeps provider wiring and model selection separate. Local agent execution resolves provider and model selection through this chain:

```text
AgentSpec.model_id
  -> ModelRegistry[model id]
  -> ModelBinding { provider_id, upstream_model }
  -> ProviderRegistry[provider id]
  -> Arc<dyn LlmExecutor>
  -> InferenceRequest.upstream_model = upstream_model
```

Endpoint-backed agents skip this local provider/model chain. They are resolved as non-local `ResolvedExecution` values and executed by the configured `ExecutionBackend`.

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

```rust
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::{AgentRuntimeBuilder, AgentSpec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
  "timeout_secs": 300,
  "adapter_options": {}
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

### ProviderSpec fields

| Field | Type | Default | Description |
|---|---|---|---|
| `id` | `String` | required | Provider identifier referenced by `ModelBindingSpec.provider_id` |
| `adapter` | `String` | required | GenAI adapter kind (e.g. `"openai"`, `"anthropic"`, `"ollama"`) |
| `api_key` | `Option<RedactedString>` | `None` | Wrapped in `RedactedString`; redacted in `Debug`/`Display`. Wire format is a plain JSON string. Empty-string input deserializes as `None` so a stored key is preserved when the field is omitted on update |
| `base_url` | `Option<String>` | `None` | Override base URL for proxies or self-hosted deployments. Empty-string input deserializes as `None` |
| `timeout_secs` | `u64` | `300` | Request timeout in seconds |
| `adapter_options` | `BTreeMap<String, Value>` | `{}` | Adapter-specific non-secret options. Today the OpenAI-compatible adapter recognizes `headers` (an object of string→string pairs added as default request headers). Unknown keys are accepted by the schema and ignored at build time. Secrets must use `api_key`; do not store credentials here |

`ProviderSpec` deserialization ignores unknown top-level fields for stored-config
compatibility. Config write and validate surfaces call `validate_provider_spec`
and reject unknown fields so new records cannot persist silently ignored
settings. Use `validate_model_binding_spec` for the same canonical validation on
model bindings.

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

## Provider secrets via the config API

The config API treats `api_key` as write-only:

- list/get responses replace `api_key` with `has_api_key: true|false`;
- `PUT` with `api_key` omitted keeps the stored key;
- `PUT` with `api_key: null` or `api_key: ""` clears it.

The in-memory representation is `RedactedString` (see
[config reference — secret handling](/config/#secret-handling)).

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
| `remove_provider(id, policy)` | Remove a provider after checking dependent model bindings and agents |

`ProviderRemovalPolicy::BlockIfReferenced` rejects removal while any model
binding points at the provider. `ProviderRemovalPolicy::CascadeUnusedModelBindings`
also removes model bindings that point at the provider, but only when no agent
uses those bindings. `ProviderRemovalPreview` reports the provider id,
referencing `model_ids`, affected `agent_ids`, and whether each policy is
currently allowed. On success, `ProviderRemovalImpact` reports the provider id
and removed model binding ids; on dependency conflicts,
`RegistryUpdateError::ProviderInUse` includes the referenced model and agent ids.

Use `rebuild_agent_model_provider_registries(base, update)` when a config source
has produced a full replacement set for agents, models, and providers. It
preserves tools, plugins, and execution backends from the base registry set,
then validates the candidate before returning it.

Diagnostics are available without publishing a snapshot:

| Function | Reports |
|---|---|
| `diagnose_registry_set(registries)` | Missing model bindings, providers, plugins, and delegate agents |
| `diagnose_registry_set_serializable(registries)` | Same diagnostics as stable payloads with `code`, `severity`, `resource`, optional `depends_on`, and `message` |
| `validate_registry_set(registries)` | Same checks as an error result |
| `diagnose_agent_spec(registries, spec)` | Problems for one agent against an existing registry set |
| `validate_agent_spec(registries, spec)` | Same agent checks as an error result |

## Inference overrides

`InferenceOverride.upstream_model` and `InferenceOverride.fallback_upstream_models` use upstream model names for the already resolved provider. They do not re-resolve `AgentSpec.model_id` and do not switch provider executors.

At execution time the primary override is applied to `InferenceRequest.upstream_model`; executors should treat that field as the single source of truth for the primary upstream model. Remaining override fields carry generation parameters and fallback upstream models.

Use model overrides for same-provider model changes:

```rust
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

## Related

- [Configure Agent Behavior](/how-to/configure-agent-behavior/)
- [Config](/config/)
- [Agent Resolution](/explanation/agent-resolution/)
