---
title: "Hot-Tune Prompts"
description: "The config-first inner loop: edit a prompt, reminder, or permission rule via the config API, and have the next run see it — no rebuild, no restart."
---

The Awaken runtime separates tools (Rust) from prompts, reminders, permissions, and skill catalogs (config). This page shows the loop you actually use to iterate on the config side, without rebuilding the binary.

## Goal

Change an agent's behaviour mid-flight and verify the change on the very next run.

## Prerequisites

- The Awaken server is running with a `ConfigStore` wired into `AppState` (see [Expose HTTP SSE](/how-to/expose-http-sse/)).
- At least one agent, model, and provider exist in config (see [Configure Agent Behavior](/how-to/configure-agent-behavior/)).
- Tools you want the agent to call are registered in Rust (`AgentRuntimeBuilder::with_tool`, see [Add a Tool](/how-to/add-a-tool/)).

## The loop

### 1. Inspect the current spec

```bash
curl -sS http://localhost:3000/v1/config/agents/research-assistant | jq .
```

The response is the spec the runtime will hand to the next run that names this `agent_id`.

### 2. Edit the part you want to change

PUT the same id with the modified fields. The example below tightens the prompt and narrows the tool surface:

```bash
curl -sS -X PUT http://localhost:3000/v1/config/agents/research-assistant \
  -H 'content-type: application/json' \
  -d '{
    "id": "research-assistant",
    "model_id": "research-default",
    "system_prompt": "You are a skeptical research assistant. Refuse to answer without at least two independent peer-reviewed sources; cite each.",
    "max_rounds": 12,
    "plugin_ids": ["permission", "reminder"],
    "allowed_tools": ["web_search", "read_document"],
    "sections": {
      "reminder": {
        "rules": [{
          "tool": "read_document",
          "output": "any",
          "message": {
            "target": "suffix_system",
            "content": "If the document is not peer-reviewed, mention that explicitly in your answer."
          }
        }]
      }
    }
  }'
```

The PUT response is the validated published config. The server compiles the change into a candidate registry snapshot, validates section schemas, then publishes — atomically, in one step.

### 3. Run and observe

```bash
curl -sS -X POST http://localhost:3000/v1/runs \
  -H 'content-type: application/json' \
  -d '{
    "agent_id": "research-assistant",
    "thread_id": "tune-2",
    "messages": [{"role": "user", "content": "Find one source on coral bleaching."}]
  }' | jq -r '.response'
```

Use a **fresh `thread_id`** to isolate the change from prior-turn context. The new prompt, reminder, and tool surface are all active.

To compare before/after rigorously, run step 3 twice with the same user message — once before the PUT in step 2, once after.

## What you can tune live

Everything below lives in config and reloads on the next run:

| Knob | Where | Effect |
|---|---|---|
| `system_prompt` | `AgentSpec.system_prompt` | Agent persona / instructions |
| `allowed_tools` / `excluded_tools` | `AgentSpec.*_tools` | Tool whitelist / blacklist |
| `max_rounds`, `reasoning_effort` | `AgentSpec.*` | Loop bounds |
| `context_policy` | `AgentSpec.context_policy` | Context window shaping + compaction |
| Permission rules | `sections.permission.rules` | Per-tool allow/ask/deny on name + args |
| Reminder rules | `sections.reminder.rules` | Inject system/conversation messages on tool patterns |
| Retry / fallback models | `sections.retry` | Same-provider model fallbacks |
| Deferred tool gating | `sections.deferred_tools` | Which tools stay eager vs load on demand |
| Compaction summarizer | `sections.compaction` | Summarizer prompt + model + threshold |
| Generative UI catalog | `sections.generative-ui` | A2UI catalog id + examples |
| Skills on disk | `~/.awaken/skills/` (or your skill root) | Auto-reloaded if `start_periodic_refresh()` was called at boot |
| MCP server tools | Remote MCP server | Auto-refreshed on `tools/list_changed` |

Anything not in this list is code: tools, plugins, provider factories, custom `PluginConfigKey` types, `Tool` trait implementations.

## Trace-driven comparison

The admin console renders the persistent trace store. To validate a tune:

1. Note the trace id of the pre-tune run.
2. PUT the new config.
3. Re-run with the same user message and a new `thread_id`. Note the new trace id.
4. Open both traces side-by-side. Compare: tool calls, gate decisions, LLM token counts, total wall time.

Traces include the prompt and section values at run start, so you have a permanent record of what produced each result.

## Active runs vs new runs

The runtime guarantees: **a run that has already started keeps its starting snapshot until it terminates.** This is the contract that makes hot-tuning safe — you cannot accidentally change a long-running agent mid-flight by editing config.

To validate a tune without waiting for active runs to drain, start a new run with a fresh `thread_id`. To rotate all agents onto the new spec, cancel and restart the active runs.

## What you can't tune live

| Change | Requires |
|---|---|
| Adding a new Rust tool implementation | Rebuild + redeploy |
| Adding a new plugin trait implementation | Rebuild + redeploy |
| Adding a new `PluginConfigKey` schema | Rebuild + redeploy |
| Swapping the `ConfigStore` backend | Restart |

If the tune you need crosses one of these lines, you're not on the hot-tune path — you're on the build-and-deploy path.

## Related

- [Configure Agent Behavior](/how-to/configure-agent-behavior/) — full config surface reference
- [Add a Tool](/how-to/add-a-tool/) — what stays in code
- [Enable Tool Permission HITL](/how-to/enable-tool-permission-hitl/) — `permission` section deep dive
- [Use Reminder Plugin](/how-to/use-reminder-plugin/) — `reminder` section deep dive
- [Use Skills Subsystem](/how-to/use-skills-subsystem/) — turning on `start_periodic_refresh`
- [Design Philosophy](/explanation/philosophy/) — why this split exists
