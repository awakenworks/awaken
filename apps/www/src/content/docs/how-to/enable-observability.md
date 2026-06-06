---
title: "Enable Observability"
description: "Operate with the Admin Console: inspect dashboard health, runtime stats, recent runs, traces, datasets, and eval results after the server wires observability stores."
---

Observability has two parts: server wiring and operator review. This guide
focuses on what the operator should see and click after the server exposes
runtime stats, traces, audit, and eval stores.

## What you click first

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-dashboard-health.png">
      <img src="/awaken/assets/admin-console/flow-dashboard-health.png" alt="Dashboard with workload, health, recent activity, and system metadata." loading="lazy" />
    </a>
    <figcaption>Dashboard: confirm optional observability subsystems are wired.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agents-list.png">
      <img src="/awaken/assets/admin-console/flow-agents-list.png" alt="Agents list showing model/plugin metadata and runtime inference statistics." loading="lazy" />
    </a>
    <figcaption>Agents list: inspect runtime stats before drilling into one agent.</figcaption>
  </figure>
</div>

1. Open **Dashboard**.
2. Check **Health** and **System** for audit log, runtime stats, trace, and eval
   availability.
3. Open **Agents** and look for inference counts, latency, and error signals.
4. Open a saved agent and use **Recent runs** when trace routes are enabled.
5. Save important traces as dataset fixtures.
6. Run an eval and inspect the report before accepting a behavior change.

## What each surface tells you

| Surface | Use it for |
|---|---|
| **Dashboard** | Health, workload, recent audit activity, and subsystem availability. |
| **Agents list** | Per-agent runtime signals such as inference count, errors, and latency when stats are wired. |
| **Recent runs / traces** | Tool calls, model output, prompt variants, and final status for a run. |
| **Datasets** | Curated traces that become repeatable fixtures. |
| **Eval Runs** | Live or scripted replay results. |
| **Eval Reports** | Offline NDJSON report review and baseline comparison. |

## Evaluate what you observed

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-datasets.png">
      <img src="/awaken/assets/admin-console/flow-datasets.png" alt="Datasets screen with dataset list and fixture counts." loading="lazy" />
    </a>
    <figcaption>Datasets: group traces into replayable fixtures.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-eval-run-detail.png">
      <img src="/awaken/assets/admin-console/flow-eval-run-detail.png" alt="Eval run detail page with pass/fail fixture output." loading="lazy" />
    </a>
    <figcaption>Eval run detail: inspect pass/fail output for each fixture.</figcaption>
  </figure>
</div>

Use evals whenever a dashboard or trace observation leads to a tuning change.
The loop is: observe → tune one field → validate → preview → save → rerun the
same fixture.

## Server wiring checklist

If a screen says a subsystem is unavailable, the UI is working: the server has
not exposed that store or route yet.

| Missing in UI | Server side to check |
|---|---|
| Empty History or Audit Log disabled | Audit log store/wiring |
| Agent stats show `n/a` | Runtime stats registry/store |
| No recent runs or trace drawer | Trace capture/store routes |
| Dataset/eval pages disabled | Eval dataset and run stores |
| No external telemetry spans | OTel exporter and collector configuration |

## Developer wiring references

When building a custom observability backend, keep the UI flow above tied to
the code surfaces that feed it:

- `MetricsSink` receives runtime metrics; use `CompositeSink` or `BatchingSink`
  when one run should feed several destinations.
- `TraceStore` backs recent runs, trace drawers, and dataset fixture curation.
- `RuntimeStatsRegistry` feeds per-agent counts, latency, and error signals.
- `SamplingPolicy` controls which spans are retained before they reach durable
  storage.

Code references: `crates/awaken-ext-observability/src/sink.rs`,
`crates/awaken-ext-observability/src/trace_store/`,
`crates/awaken-ext-observability/tests/observability_integration.rs`, and
`crates/awaken-ext-observability/tests/wiring_integration.rs`.

## API references for automation

- [Admin Console surface inventory](/awaken/reference/admin-console/)
- [HTTP API](/awaken/reference/http-api/)
- [Events](/awaken/reference/events/)
