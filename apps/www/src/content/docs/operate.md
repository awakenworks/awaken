---
title: "Tune & Operate"
description: "Use the Admin Console and config plane to tune saved agents, inspect runs, curate traces, and harden production behavior."
---

This path is for the product surface around a running Awaken server. Developers
still implement executable capability in Rust, but operators tune the managed
parts online: prompts, tool descriptions, models, model pools, MCP servers,
skills, delegates, reminders, deferred-tool policy, permission rules, traces,
datasets, and evals.

The Admin Console is the primary UI for this path. The REST config API is the
same control plane for CI or internal tooling.

## Recommended order

1. [Use the Admin Console](/awaken/how-to/use-admin-console/) to connect a
   running server, configure provider-backed models, create agents, preview
   drafts, and publish the next registry snapshot.
2. [Configure Agent Behavior](/awaken/how-to/configure-agent-behavior/) and
   [Hot-Tune Prompts](/awaken/how-to/hot-tune-prompts/) for the full editable
   surface.
3. [Use MCP Tools](/awaken/how-to/use-mcp-tools/), [Use Skills Subsystem](/awaken/how-to/use-skills-subsystem/),
   [Use Reminder Plugin](/awaken/how-to/use-reminder-plugin/), and
   [Use Deferred Tools](/awaken/how-to/use-deferred-tools/) when the agent needs
   discoverable or delayed capabilities.
4. [Enable Observability](/awaken/how-to/enable-observability/) and
   [Report Tool Progress](/awaken/how-to/report-tool-progress/) to make runs,
   tools, and providers visible.
5. [Enable Tool Permission HITL](/awaken/how-to/enable-tool-permission-hitl/) and
   [Configure Stop Policies](/awaken/how-to/configure-stop-policies/) to keep
   agent behavior bounded and reviewable.
6. [Testing Strategy](/awaken/how-to/testing-strategy/) and
   [Recover Streaming LLMs](/awaken/how-to/recover-streaming-llms/) cover
   regression confidence and provider failure handling.

## Replay and eval loop

`awaken-eval` replays saved fixtures through `RuntimeReplayer`, scores the
outputs, and diffs them against NDJSON baselines. Use it for regression checks
over saved prompts, tool outputs, and provider scripts without paying live
provider cost. Trace curation helpers can turn captured runs into fixtures;
live mode remains available when provider drift is the behavior you want to
measure.

## Harden the admin and config plane

Two orthogonal levers, both detailed in the
[config reference](/awaken/reference/config/):

- `AdminApiConfig.bearer_token` (or `AWAKEN_ADMIN_API_BEARER_TOKEN`) protects
  `/v1/capabilities`, `/v1/config/*`, `/v1/agents*`, `/v1/system/info`,
  `/v1/audit-log`, and runtime-stats endpoints.
- `AdminApiConfig.expose_config_routes = false` drops the admin CRUD routes
  entirely when configuration is owned by an external pipeline.

For storms of small config writes, set
`ConfigRuntimeManager::with_min_apply_interval(Duration)` to coalesce
listener-driven applies; cached `ProviderSpec` executors are reused across
unchanged specs.

## Keep nearby

- [Errors](/awaken/reference/errors/)
- [Cancellation](/awaken/reference/cancellation/)
- [HITL and Mailbox](/awaken/explanation/hitl-and-mailbox/)
- [Config](/awaken/reference/config/)
