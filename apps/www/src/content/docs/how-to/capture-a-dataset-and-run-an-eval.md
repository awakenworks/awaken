---
title: "Capture a Dataset and Run an Eval"
description: "Use the Admin Console to turn observed traces into dataset fixtures, run an eval, and inspect pass/fail results before accepting a tuning change."
---

Use evals when a prompt, model, tool, permission, or stop-policy change should
be checked against the same examples every time. The browser flow is: observe a
run, save useful traces as fixtures, run the dataset, then inspect results.

## What you click

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-datasets.png">
      <img src="/awaken/assets/admin-console/flow-datasets.png" alt="Datasets screen with dataset list, fixture counts, and create actions." loading="lazy" />
    </a>
    <figcaption>Datasets: group trace fixtures into a replayable suite.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-eval-run-detail.png">
      <img src="/awaken/assets/admin-console/flow-eval-run-detail.png" alt="Eval run detail page with pass rate, failures, and per-fixture reports." loading="lazy" />
    </a>
    <figcaption>Eval run detail: review pass/fail output fixture by fixture.</figcaption>
  </figure>
</div>

## 1. Produce a trace

1. Run the agent from your client or from the agent editor preview.
2. Open the saved agent.
3. Use **Recent runs** to inspect the trace when trace routes are enabled.
4. Choose a run that captures behavior you want to keep or compare.

If the trace drawer is unavailable, wire trace storage first. See
[Enable Observability](/awaken/how-to/enable-observability/).

## 2. Create or choose a dataset

1. Open **Observe → Datasets**.
2. Click **New Dataset** when this is a new behavior suite, or open an existing
   dataset for a regression set.
3. Use a stable id that describes the behavior, such as
   `research-citations` or `tool-permission`.

## 3. Add fixtures from traces

1. From the trace drawer, click **Save as fixture**.
2. Choose the dataset.
3. Give the fixture a readable id and description.
4. Keep provider-script capture required when the run must be replayable without
   spending model tokens.
5. Save and confirm the dataset fixture count changed.

## 4. Run the eval

1. Open the dataset.
2. Click **Run eval**.
3. Choose the agent and model context you want to test.
4. Start the run.
5. Open the generated **Eval Run**.

Use scripted mode when fixtures contain provider scripts. Use live mode only
when you intentionally want to call the configured model provider.

## 5. Read the results

Check:

- pass rate and failure count;
- per-fixture final answer;
- expectation/check failures;
- baseline differences if you uploaded or selected a baseline report.

A failed fixture is a tuning input: return to the agent editor, change one
field, validate, preview, save, then rerun the same eval.

## What the console calls

Endpoint details belong in the references. For automation, see:

- [HTTP API](/awaken/reference/http-api/)
- [Admin Console surface inventory](/awaken/reference/admin-console/)
- [Enable Observability](/awaken/how-to/enable-observability/)
