---
title: "Configure Agent Behavior"
description: "Use managed configuration when the same server binary should host multiple agent profiles, switch model configs or model pools, or tune plugin behavior without changing Rust code. Keep new tools, new plugins, and…"
---

Awaken is built around a **two-layer split**:

- **Code (Rust, compiled once)** — tools, plugins, custom provider factories,
  storage backends.
- **Config (declarative, hot-swappable)** — `ProviderSpec` and `ModelSpec` records, model pools, agents,
  MCP servers, skills, and typed `AgentSpec.sections`.

This guide is the canonical "tune at runtime" reference. Once your tools are
written and the runtime is up, *almost every behavioural change you make to an
agent is a config edit* — system prompt wording, model choice, tool description
overrides, allowed tools, reasoning effort, context policy, reminder cadence,
ToolSearch/deferred-tool policy, skill activation metadata, and delegates. The
same server binary hosts many agent profiles; switching profiles is a
`PUT /v1/config/agents/:id` (or a Save in the
[Admin Console](/awaken/reference/admin-console/)), not a redeploy.

**Treat configuration as the optimization surface.** The loop is: edit spec →
Validate → Save → run a preview chat → measure → adjust. The runtime
swaps to the new spec on the next request without restarting the process.

This guide assumes the server has a `ConfigStore` wired into `ServerState` and that
the referenced plugins have been registered in the runtime plugin registry.

## Configuration layers

| Layer | Where it lives | Use it for |
|---|---|---|
| Provider | `/v1/config/providers/{id}` | Adapter, API key source, base URL, timeout |
| Model config | `/v1/config/models/{id}` | Stable model id -> `ModelSpec` with provider id, upstream model name, capabilities, and pricing |
| Model pool | `/v1/config/model-pools/{id}` | Stable pool id -> ordered `ModelSpec` members with sticky routing and failover policy |
| Agent | `/v1/config/agents/{id}` | Prompt, stable `model_id`, rounds, tools, plugins, context policy |
| Tool override | `/v1/config/tools/{id}/overrides` | Runtime-safe tool description override |
| MCP server | `/v1/config/mcp-servers/{id}` | External MCP server connections |
| Skill | `/v1/config/skills/{id}` | Reusable instructions, arguments, and allowed tools |
| Plugin section | `AgentSpec.sections` | Per-agent typed config keyed by `PluginConfigKey::KEY` |
| Runtime code | `AgentRuntimeBuilder` | Register tools, provider factories, plugins, backends |

Provider adapters supported by the managed config runtime are returned by
`GET /v1/capabilities` as `supported_adapters`. The list is derived from the
linked `genai` version at runtime, so new upstream adapters appear there after
the dependency supports them.

## Resolution model

Local agent execution resolves model and provider configuration through stable
registry ids:

```text
AgentSpec.model_id
  -> ModelSpec { provider_id, upstream_model }
  -> ProviderSpec { adapter, api_key, base_url, timeout_secs }
  -> LlmExecutor
```

`AgentSpec.model_id` is not the upstream provider model name. It is the stable
`ModelSpec.id` used by agents and clients. `ModelSpec.upstream_model`
is the string sent to the provider API.

Config writes are compiled into a candidate registry snapshot, validated, and
then published. New runs use the latest published snapshot. Runs that already
started keep the snapshot they started with. Audit-log restore is the exception:
it writes the recovered payload to the editing store only; publish that payload
with a normal config save/PUT when it should become active for new runs.

If the deployment uses a `VersionedRegistryStore`, published runtime snapshots
are immutable and durable runs carry a `resolution_id` that reselects the same
published graph for resume/replay. Record revisions and audit restore make
config history traceable; they are separate from manually pinning an arbitrary
editing-store version as production.

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

Create a `ModelSpec` that binds a stable model id to that provider:

```bash
curl -sS -X PUT http://localhost:3000/v1/config/models/research-default \
  -H 'content-type: application/json' \
  -d '{
    "id": "research-default",
    "provider_id": "anthropic-prod",
    "upstream_model": "claude-sonnet-4-20250514"
  }'
```

For provider or quota failover, add a second `ModelSpec`, create a
`ModelPoolSpec`, and point the agent at the pool id instead of a single model:

```bash
curl -sS -X PUT http://localhost:3000/v1/config/models/research-fallback \
  -H 'content-type: application/json' \
  -d '{
    "id": "research-fallback",
    "provider_id": "anthropic-prod",
    "upstream_model": "claude-3-5-haiku-20241022"
  }'

curl -sS -X PUT http://localhost:3000/v1/config/model-pools/research-pool \
  -H 'content-type: application/json' \
  -d '{
    "id": "research-pool",
    "members": [
      { "model_id": "research-default", "weight": 3 },
      { "model_id": "research-fallback", "role": "failover_only" }
    ],
    "routing": {
      "home": "deterministic",
      "sticky_scope": "thread"
    },
    "switch": {
      "on_circuit_open": true,
      "on_quota": true,
      "quota_retry_after_threshold_secs": 10,
      "max_switches_per_session": 2
    }
  }'
```

Create an agent that references the stable model or pool id:

```bash
curl -sS -X PUT http://localhost:3000/v1/config/agents/research-assistant \
  -H 'content-type: application/json' \
  -d '{
    "id": "research-assistant",
    "model_id": "research-pool",
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
      "summarizer_user_prompt": "Update the cumulative conversation summary.\n\n<existing-summary>\n{previous_summary}\n</existing-summary>\n\n<new-conversation>\n{messages}\n</new-conversation>",
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
| `retry` | Resolver | Retry/backoff policy for the selected model or pool member. |
| `permission` | Permission plugin | Default allow/ask/deny behavior plus ordered tool rules. |
| `reminder` | Reminder plugin | Tool/output matching rules that inject system or conversation context. |
| `generative-ui` | Generative UI plugin | A2UI catalog id, examples, or full prompt instructions. |
| `deferred_tools` | Deferred tools plugin | Decide which schemas stay eager, which are found through `ToolSearch`, and when promoted tools are re-deferred. |
| `skills` | Skills discovery plugin | Optional skill allowlist for the injected skill catalog. Skill content and activation metadata live in `SkillSpec`. |
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

Today, ToolSearch is implemented only by the deferred-tools plugin. Skills are
catalog-injected and activated through the `skill` tool; there is no separate
`SkillSearch` tool. Sub-agents are explicit `AgentSpec.delegates`; there is no
separate `AgentSearch` tool.

Leave `active_hook_filter` empty for normal agents. A non-empty filter disables
hooks, plugin tools, and request transforms from plugins whose descriptor names
are not listed; it is mainly useful when deliberately narrowing active behavior
for a specific agent.

## Tuning workflow

1. Choose stable `providers`, `models`, and `agents` ids first. Let clients call
   agents by agent id, and let agents refer to stable `ModelSpec.id` values.
2. Change `ModelSpec.upstream_model` for same-provider model updates. Use a
   different `ModelSpec` or a `ModelPoolSpec` when the provider should change.
3. Tune broad loop behavior with `system_prompt`, `max_rounds`,
   `max_continuation_retries`, `reasoning_effort`, and `context_policy`.
4. Restrict visible tools with `allowed_tools` and `excluded_tools`.
5. Add `permission` rules for runtime allow/ask/deny decisions.
6. Point `model_id` at a model pool when the agent needs model failover.
7. Add `reminder`, `generative-ui`, `deferred_tools`, and `compaction` sections
   only when the corresponding plugin behavior is needed.
8. Publish the config through `/v1/config/*`, then start a new run to use the
   new snapshot.

## Compatibility rules

- Keep `AgentSpec.id`, `ModelSpec.id`, and `ProviderSpec.id` stable for
  clients that reference them.
- Use canonical fields: `model_id`, `provider_id`, and `upstream_model`.
  Legacy `model` and `provider` fields are not managed config fields.
- Treat `InferenceOverride.upstream_model` as a same-provider override. It does
  not re-resolve `AgentSpec.model_id`, cannot switch provider executors, and
  is rejected for model-pool-backed agents.
- Query `/v1/config/{namespace}/$schema` before writing generated config, and
  use `/v1/capabilities` to inspect plugin `config_schemas`. `AgentSpec`,
  `ModelSpec`, and several section types reject unknown fields.
- Additive section changes are compatible when the plugin is registered and the
  section value matches the schema. Invalid sections fail validation before the
  runtime snapshot is published.
- Removing a plugin id while leaving its section in place does not activate that
  plugin; unresolved section keys are logged as possible typos.
- Active runs keep their starting snapshot. To validate a change, create a new
  run after the config write succeeds.

## Verify the loop — edit, run, observe

The config plane only matters if changes really land. This is the proof loop. The server stays running throughout.

### 1. Run with the original prompt

```bash
curl -sS -X POST http://localhost:3000/v1/runs \
  -H 'content-type: application/json' \
  -d '{
    "agent_id": "research-assistant",
    "thread_id": "verify-thread",
    "messages": [{"role": "user", "content": "Find one peer-reviewed source on coral bleaching."}]
  }' | jq -r '.response'
```

Note the tone, citation behaviour, and tool choice in the response.

### 2. Edit only the prompt — same id, no restart

```bash
curl -sS -X PUT http://localhost:3000/v1/config/agents/research-assistant \
  -H 'content-type: application/json' \
  -d '{
    "id": "research-assistant",
    "model_id": "research-default",
    "system_prompt": "You are a skeptical research assistant. Refuse to answer without at least two independent peer-reviewed sources; cite each.",
    "max_rounds": 12,
    "plugin_ids": ["permission"],
    "allowed_tools": ["web_search", "read_document", "summarize"]
  }'
```

The PUT returns the validated, published config. No build, no redeploy, no SIGHUP.

### 3. Run again — observe the change

```bash
curl -sS -X POST http://localhost:3000/v1/runs \
  -H 'content-type: application/json' \
  -d '{
    "agent_id": "research-assistant",
    "thread_id": "verify-thread-2",
    "messages": [{"role": "user", "content": "Find one peer-reviewed source on coral bleaching."}]
  }' | jq -r '.response'
```

The new run picks up the snapshot published in step 2. Compare the two responses — the second should now require two sources and refuse single-source answers.

Use a fresh `thread_id` to avoid prior-turn carryover; the prompt change is the only independent variable.

### What is safe to change mid-run

| Change | Active runs | Next run |
|---|---|---|
| `system_prompt` | Keep old prompt | New prompt |
| `allowed_tools`, `excluded_tools` | Keep old set | New set |
| `max_rounds`, `reasoning_effort` | Keep old | New |
| `model_id` (swap binding only) | Keep old binding | New binding |
| `plugin_ids` (add) | Keep old set | New plugin runs from next round |
| `plugin_ids` (remove) | Keep old set | Removed plugin's hooks stop firing |
| `sections.<plugin>` (validated by `PluginConfigKey`) | Keep old | New per-key validated value |

Active runs always finish on their starting snapshot — this is the contract. To validate a change without waiting for active runs to drain, start a new run with a fresh `thread_id`.

## Related

- [Provider and Model Configuration](/awaken/reference/provider-model-config/)
- [Config](/awaken/reference/config/)
- [HTTP API](/awaken/reference/http-api/)
- [Hot-Tune Prompts](/awaken/how-to/hot-tune-prompts/)
- [Enable Tool Permission HITL](/awaken/how-to/enable-tool-permission-hitl/)
- [Use Reminder Plugin](/awaken/how-to/use-reminder-plugin/)
- [Use Deferred Tools](/awaken/how-to/use-deferred-tools/)
- [Optimize Context Window](/awaken/how-to/optimize-context-window/)
