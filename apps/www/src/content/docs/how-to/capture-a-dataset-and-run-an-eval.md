---
title: "Capture a Dataset and Run an Eval"
description: "Turn real traces into a regression dataset, run it against an agent on a real or scripted model, and read per-fixture pass/fail with baseline diffs — all from the Admin Console."
---

Evaluation in Awaken is a loop: capture real runs as **fixtures** in a
**dataset**, **run** that dataset against an agent and model, then read a
per-fixture **report**. Runs can be *scripted* (deterministic replay of recorded
provider events — fast, no tokens) or *live* (re-execute against a real model).
This guide walks the full loop from the browser.

## Prerequisites

- A running `awaken-server` with a `ConfigStore` and the **trace routes**
  enabled (`AWAKEN_EXPOSE_TRACE_ROUTES=true`) so runs are captured.
- An agent you can exercise, and — for live runs — a provider-backed model.

## 1. Produce a trace

Run the agent so there's something to capture. Open the agent editor and send a
message in the **Sandbox** (draft-preview chat), or drive it over the API. With
trace routes on, the run is recorded in the trace store.

## 2. Create a dataset

1. Open **Datasets** (sidebar → **Observe**) and click **+ New Dataset**.
2. Give it a **Dataset ID** (alphanumeric, `-`, `_`; ≤64 chars) and an optional
   **Description**, then click **Create**. This calls
   `POST /v1/eval/datasets` with an empty `fixtures` list.

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/datasets.png">
    <img src="/awaken/assets/admin-console/datasets.png" alt="The Datasets screen under Observe, with a '+ New Dataset' button and an empty state explaining that a dataset groups trace fixtures to replay as an eval suite." loading="lazy" />
  </a>
  <figcaption>Datasets group trace fixtures into a replayable eval suite.</figcaption>
</figure>

## 3. Add fixtures from traces

A fixture is a recorded run plus an **expectation**.

1. Open a recent run/trace and choose **Save trace as fixture**.
2. Pick the target dataset, give the fixture an id/description, and set the
   expectation — typically **must include** / **must exclude** substrings in the
   final answer (you can also assert a `tool_sequence` or a minimum judge score).
3. Save. The console calls `POST /v1/eval/datasets/:id/items` with
   `{ from_run_id, expected, … }`. The backend pulls the user input and, unless
   you skip it, records the provider events as a replayable `provider_script`.

A fixture saved with `provider_script_mode: "skip"` is **live-only** (it has no
scripted replay and must be run against a real model).

## 4. Run the eval

**Scripted (fast, deterministic):** on the dataset detail page click **Run**.
Each fixture replays its recorded `provider_script` — no tokens, ideal for CI.
(Disabled if every fixture is live-only.)

**Live (real model):** start a run with a model matrix:

```http
POST /v1/eval/runs
{ "dataset_id": "my-dataset", "mode": "live",
  "agent_id": "concierge", "models": ["default"] }
```

- `mode` — `live` or `scripted`. Omit it and the server infers `live` when
  `models` is present, else `scripted`.
- `models` — the model ids to evaluate; each `(fixture, model)` pair is one
  cell. `["default"]` runs every fixture once on `default`.
- `agent_id` — the registered agent whose config (prompt, tools, sampling) is
  the base for live replays. Optional `agent_overrides` patches it per run.
- Optional: `samples` (flakiness sampling), `judge` (LLM-as-judge),
  `baseline_run_id` (diff against a prior run), `max_walltime_secs`,
  `max_total_tokens`.

The response is an `EvalRun` whose `items[*].report.passed` carries the
per-fixture verdict.

## 5. Read the results

- **Eval Runs** lists runs (`GET /v1/eval/runs?dataset_id=…`) with mode,
  fixture and failure counts. Click a run to open the detail
  (`GET /v1/eval/runs/:id`): per-fixture pass/fail, failures, tokens, duration.
- **Eval Reports** is an offline viewer — upload a report NDJSON (and an
  optional baseline) to see a pass/fail summary, per-fixture diff, and filters
  for **Regressions** / **Newly fixed**. Pass `baseline_run_id` on a run to get
  the diff computed server-side.

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/eval-run.png">
    <img src="/awaken/assets/admin-console/eval-run.png" alt="An eval run detail page: Mode live, pass rate 100%, zero failures, and a per-fixture report showing passed=true with the model's final answer." loading="lazy" />
  </a>
  <figcaption>An eval run detail — pass rate, failures, and per-fixture reports.</figcaption>
</figure>

## What the console calls

| Action | Endpoint |
|---|---|
| List / create dataset | `GET` / `POST /v1/eval/datasets` |
| Get / update / delete | `GET` / `PUT` / `DELETE /v1/eval/datasets/:id` |
| Curate fixture from trace | `POST /v1/eval/datasets/:id/items` |
| Start a run | `POST /v1/eval/runs` |
| List / get runs | `GET /v1/eval/runs`, `GET /v1/eval/runs/:id` |

## Related

- [Hot-Tune Prompts](/awaken/how-to/hot-tune-prompts/)
- [Enable Observability](/awaken/how-to/enable-observability/)
- [Use the Admin Console](/awaken/how-to/use-admin-console/)
