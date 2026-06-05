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
2. [Build an Agent with the Admin Assistant](/awaken/how-to/build-an-agent-with-the-assistant/)
   to draft an agent from a natural-language description once a model is live.
3. [Configure Agent Behavior](/awaken/how-to/configure-agent-behavior/) and
   [Hot-Tune Prompts](/awaken/how-to/hot-tune-prompts/) for the full editable
   surface.
4. [Connect an A2A Server](/awaken/how-to/connect-an-a2a-server/) to bring remote
   agents into the catalog, then
   [Capture a Dataset and Run an Eval](/awaken/how-to/capture-a-dataset-and-run-an-eval/)
   to score behavior before you ship a change.
5. [Enable Observability](/awaken/how-to/enable-observability/) to make runs,
   tools, and providers visible.
6. [Enable Tool Permission HITL](/awaken/how-to/enable-tool-permission-hitl/) and
   [Configure Stop Policies](/awaken/how-to/configure-stop-policies/) to keep
   agent behavior bounded and reviewable.

Tool, plugin, MCP, skills, and reminder *capabilities* are built in code — see
[Develop Agents](/awaken/build-agents/). This section tunes and runs what you
built.

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
