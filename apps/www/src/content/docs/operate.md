---
title: "Operate"
description: "This path is for hardening an agent service once the happy path already works."
---

This path is for hardening an agent service once the happy path already works.

## Recommended order

1. [Enable Observability](/awaken/how-to/enable-observability/) to make runs, tools, and providers visible.
2. [Enable Tool Permission HITL](/awaken/how-to/enable-tool-permission-hitl/) to add approval control over tool execution.
3. [Configure Stop Policies](/awaken/how-to/configure-stop-policies/) to keep agent loops bounded and predictable.
4. [Report Tool Progress](/awaken/how-to/report-tool-progress/) and [Testing Strategy](/awaken/how-to/testing-strategy/) to improve operator visibility and confidence.
5. [Recover Streaming LLMs](/awaken/how-to/recover-streaming-llms/) when transient provider failures must not surface as run errors.

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
