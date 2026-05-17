---
title: "Configure Agent Behavior"
description: "Use managed configuration when the same server binary should host multiple agent profiles, switch model bindings, or tune plugin behavior without changing Rust code. Keep new tools, new plugins, and…"
---

Use managed configuration when the same server binary should host multiple agent
profiles, switch model bindings, or tune plugin behavior without changing Rust
code. Keep new tools, new plugins, and custom provider factories in code; keep
provider, model, agent, MCP server, and typed section values in config.

This guide assumes the server has a `ConfigStore` wired into `AppState` and that
the referenced plugins have been registered in the runtime plugin registry.

## Configuration layers

| Layer | Where it lives | Use it for |
|---|---|---|
| Provider | `/v1/config/providers/{id}` | Adapter, API key source, base URL, timeout |
| Model binding | `/v1/config/models/{id}` | Stable model id -> provider id + upstream model name |
| Agent | `/v1/config/agents/{id}` | Prompt, model binding, rounds, tools, plugins, context policy |
| MCP server | `/v1/config/mcp-servers/{id}` | External MCP server connections |
| Plugin section | `AgentSpec.sections` | Per-agent typed config keyed by `PluginConfigKey::KEY` |
| Runtime code | `AgentRuntimeBuilder` | Register tools, provider factories, plugins, backends |

Provider adapters supported by the managed config runtime are:
`anthropic`, `openai`, `openai_resp`, `deepseek`, `gemini`, `ollama`,
`cohere`, `together`, `fireworks`, `groq`, `xai`, `zai`, `bigmodel`,
`aliyun`, `mimo`, and `nebius`.

## Resolution model

Local agent execution resolves model and provider configuration through stable
registry ids:

```text
AgentSpec.model_id
  -> ModelBindingSpec { provider_id, upstream_model }
  -> ProviderSpec { adapter, api_key, base_url, timeout_secs }
  -> LlmExecutor
```

`AgentSpec.model_id` is not the upstream provider model name. It is the stable
model binding id used by agents and clients. `ModelBindingSpec.upstream_model`
is the string sent to the provider API.

Config writes are compiled into a candidate registry snapshot, validated, and
then published. New runs use the latest published snapshot. Runs that already
started keep the snapshot they started with.

Endpoint-backed agents skip the local provider, model, plugin, and tool
resolution chain. Their `endpoint` config is executed by the selected remote
backend instead.

## Minimal managed config

Create or update a provider. When `api_key` is omitted, the provider adapter
uses its environment variable. Setting `api_key` to `null` or `""` clears a
stored key.

```bash
curl -sS -X PUT http://localhost:3000/v1/config/providers/anthropic-prod \
  -H 'content-type: application/json' \
  -d '{
    "id": "anthropic-prod",
    "adapter": "anthropic",
    "base_url": null,
    "timeout_secs": 300
  }'
```

Bind a stable model id to that provider:

```bash
curl -sS -X PUT http://localhost:3000/v1/config/models/research-default \
  -H 'content-type: application/json' \
  -d '{
    "id": "research-default",
    "provider_id": "anthropic-prod",
    "upstream_model": "claude-sonnet-4-20250514"
  }'
```

Create an agent that uses the model binding:

```bash
curl -sS -X PUT http://localhost:3000/v1/config/agents/research-assistant \
  -H 'content-type: application/json' \
  -d '{
    "id": "research-assistant",
    "model_id": "research-default",
    "system_prompt": "You help with source-grounded research. Ask before using destructive tools.",
    "max_rounds": 12,
    "reasoning_effort": "medium",
    "plugin_ids": ["permission"],
    "allowed_tools": ["web_search", "read_document", "summarize"],
    "context_policy": {
      "max_context_tokens": 120000,
      "max_output_tokens": 8192,
      "min_recent_messages": 8,
      "enable_prompt_cache": true,
      "autocompact_threshold": 90000,
      "compaction_mode": "keep_recent_raw_suffix",
      "compaction_raw_suffix_messages": 2
    }
  }'
```

## Tune with sections

`AgentSpec.sections` carries typed plugin or resolver config. Keys are stable
strings declared by `PluginConfigKey::KEY`; values must match the schema for the
consumer that reads that key.

```json
{
  "sections": {
    "retry": {
      "max_retries": 2,
      "fallback_upstream_models": ["claude-3-haiku"],
      "backoff_base_ms": 500
    },
    "permission": {
      "default_behavior": "ask",
      "rules": [
        { "tool": "read_document", "behavior": "allow" },
        { "tool": "web_search", "behavior": "ask" },
        { "tool": "delete_*", "behavior": "deny" }
      ]
    },
    "reminder": {
      "rules": [
        {
          "tool": "Edit(file_path ~ '*.toml')",
          "output": "any",
          "message": {
            "target": "suffix_system",
            "content": "You edited a TOML file. Run cargo check before finishing."
          }
        }
      ]
    },
    "generative-ui": {
      "catalog_id": "https://a2ui.org/specification/v0_8/standard_catalog_definition.json",
      "examples": "Use compact components for status summaries and forms."
    },
    "deferred_tools": {
      "enabled": true,
      "default_mode": "deferred",
      "rules": [
        { "tool": "summarize", "mode": "eager" }
      ],
      "beta_overhead": 1136.0
    },
    "compaction": {
      "summarizer_system_prompt": "You are a conversation summarizer. Preserve decisions, facts, tool results, and unresolved tasks.",
      "summarizer_user_prompt": "Summarize the following conversation:\n\n{messages}",
      "summary_max_tokens": 1024,
      "summary_model": "claude-3-haiku",
      "min_savings_ratio": 0.3
    }
  }
}
```

Common keys:

| Key | Consumer | Notes |
|---|---|---|
| `retry` | Resolver | Retry and same-provider fallback upstream models. |
| `permission` | Permission plugin | Default allow/ask/deny behavior plus ordered tool rules. |
| `reminder` | Reminder plugin | Tool/output matching rules that inject system or conversation context. |
| `generative-ui` | Generative UI plugin | A2UI catalog id, examples, or full prompt instructions. |
| `deferred_tools` | Deferred tools plugin | Decide which tool schemas stay eager and which are loaded on demand. |
| `compaction` | Context compaction plugin | Summarizer prompts, summary model, and accepted savings threshold. |

`context_policy` is a top-level `AgentSpec` field, not a section. Setting it
enables context transforms and context compaction. The optional `compaction`
section only tunes the summarizer used by compaction.

`plugin_ids` and section keys are different. `plugin_ids` uses plugin registry
ids such as `permission`, `reminder`, and `ext-deferred-tools`. The
`deferred_tools` section key is the config key for the deferred tools plugin.
For plugin sections, make sure the corresponding plugin id is present in
`plugin_ids`. `retry` is read by the resolver, and `compaction` is available
when `context_policy` enables the built-in context compaction plugins.

Some plugins also accept constructor defaults. For example,
`ReminderPlugin::new(rules)` installs fleet-wide default reminder rules, while a
per-agent `reminder` section is validated through `ReminderConfigKey` and
appended at runtime. Use constructor defaults for baseline behavior and
`AgentSpec.sections` for agent-specific tuning.

Leave `active_hook_filter` empty for normal agents. A non-empty filter disables
hooks, plugin tools, and request transforms from plugins whose descriptor names
are not listed; it is mainly useful when deliberately narrowing active behavior
for a specific agent.

## Tuning workflow

1. Choose stable `providers`, `models`, and `agents` ids first. Let clients call
   agents by agent id, and let agents refer to model binding ids.
2. Use model bindings to change upstream model names. Use another binding when
   the provider should change.
3. Tune broad loop behavior with `system_prompt`, `max_rounds`,
   `max_continuation_retries`, `reasoning_effort`, and `context_policy`.
4. Restrict visible tools with `allowed_tools` and `excluded_tools`.
5. Add `permission` rules for runtime allow/ask/deny decisions.
6. Add `retry` fallback upstream models for same-provider resilience.
7. Add `reminder`, `generative-ui`, `deferred_tools`, and `compaction` sections
   only when the corresponding plugin behavior is needed.
8. Publish the config through `/v1/config/*`, then start a new run to use the
   new snapshot.

## Compatibility rules

- Keep `AgentSpec.id`, `ModelBindingSpec.id`, and `ProviderSpec.id` stable for
  clients that reference them.
- Use canonical fields: `model_id`, `provider_id`, `upstream_model`, and
  `fallback_upstream_models`. Legacy `model`, `provider`, and
  `fallback_models` fields are not managed config fields.
- Treat `InferenceOverride.upstream_model` as a same-provider override. It does
  not re-resolve `AgentSpec.model_id` and cannot switch provider executors.
- Query `/v1/config/{namespace}/$schema` before writing generated config, and
  use `/v1/capabilities` to inspect plugin `config_schemas`. `AgentSpec`,
  `ModelBindingSpec`, and several section types reject unknown fields.
- Additive section changes are compatible when the plugin is registered and the
  section value matches the schema. Invalid sections fail validation before the
  runtime snapshot is published.
- Removing a plugin id while leaving its section in place does not activate that
  plugin; unresolved section keys are logged as possible typos.
- Active runs keep their starting snapshot. To validate a change, create a new
  run after the config write succeeds.

## Related

- [Provider and Model Configuration](/reference/provider-model-config/)
- [Config](/reference/config/)
- [HTTP API](/reference/http-api/)
- [Enable Tool Permission HITL](/enable-tool-permission-hitl/)
- [Use Reminder Plugin](/use-reminder-plugin/)
- [Use Deferred Tools](/use-deferred-tools/)
- [Optimize Context Window](/optimize-context-window/)
