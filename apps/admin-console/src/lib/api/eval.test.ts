import { afterEach, describe, expect, it, vi } from "vitest";

import { BACKEND_URL } from "./http";
import { evalApi, type EvalRun, type EvalRunSummary, type Expectation } from "./eval";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("evalApi wire contract", () => {
  // The Rust server emits `id`, `execution_mode`, `item_count`, `passed_count`
  // for eval-run summaries and `items[].report.passed` for run details. An
  // earlier draft of this client used `run_id`/`mode`/`fixture_count` which
  // navigated to `/eval-runs/undefined`. This suite pins the wire shape so
  // a rename on either side blows up here instead of in the UI.

  it("listRuns parses the canonical EvalRunSummary fields incl. failed_count", async () => {
    const wire = {
      runs: [
        {
          id: "01KSCSC03DKNR5K1Z07PSPZDTG",
          dataset_id: "demo-greetings",
          dataset_revision: 1,
          execution_mode: "scripted",
          started_at_secs: 1779619463,
          ended_at_secs: 1779619463,
          item_count: 3,
          passed_count: 2,
          failed_count: 1,
        },
      ],
    };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(jsonResponse(wire)));

    const result = await evalApi.listRuns();
    expect(result).not.toBeNull();
    const runs = result!.runs;
    expect(runs).toHaveLength(1);
    const summary: EvalRunSummary = runs[0];
    expect(summary.id).toBe("01KSCSC03DKNR5K1Z07PSPZDTG");
    expect(summary.execution_mode).toBe("scripted");
    expect(summary.item_count).toBe(3);
    expect(summary.passed_count).toBe(2);
    expect(summary.failed_count).toBe(1);
    // Pending = item_count - passed_count - failed_count. Today the
    // backend ensures every persisted item has a report so it's 0.
    expect(summary.item_count - summary.passed_count - (summary.failed_count ?? 0)).toBe(0);
  });

  it("listRuns tolerates older servers that omit failed_count", async () => {
    const wire = {
      runs: [
        {
          id: "01OLD",
          dataset_id: "demo",
          dataset_revision: 1,
          execution_mode: "scripted",
          started_at_secs: 1,
          item_count: 2,
          passed_count: 1,
        },
      ],
    };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(jsonResponse(wire)));
    const { runs } = (await evalApi.listRuns())!;
    expect(runs[0].failed_count).toBeUndefined();
    expect(runs[0].item_count - runs[0].passed_count).toBe(1);
  });

  it("startRun returns a run with id (not run_id) so navigate(adminRoutes.evalRun(run.id)) lands", async () => {
    const wire = {
      run: {
        id: "01KSCWTC63XY1QP761RABWTW61",
        dataset_id: "demo-greetings",
        dataset_revision: 1,
        execution_mode: "live",
        items: [
          {
            fixture_id: "greet-hello",
            report: {
              fixture_id: "greet-hello",
              passed: true,
              failures: [],
              final_text: "HELLO_AWAKEN_LIVE",
              inference_count: 1,
            },
            trace_run_id: "019e59cd-2848-7be1-8307-2c275969754e",
          },
        ],
        started_at_secs: 1779623077,
        ended_at_secs: 1779623080,
      },
    };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(jsonResponse(wire)));

    const resp = await evalApi.startRun({ dataset_id: "demo-greetings", mode: "live" });
    const run: EvalRun = resp.run;
    expect(run.id).toBe("01KSCWTC63XY1QP761RABWTW61");
    expect(run.execution_mode).toBe("live");
    expect(run.items?.[0]?.report?.passed).toBe(true);
    expect(run.items?.[0]?.report?.final_text).toBe("HELLO_AWAKEN_LIVE");
  });

  it("Expectation uses final_answer_contains (server schema) not must_include", () => {
    // The server's `Expectation` struct names this field `final_answer_contains`.
    // A prior draft sent `must_include` and the server rejected the curate
    // request with "expected must contain at least one expectation criterion".
    const ok: Expectation = { final_answer_contains: ["Hello"] };
    expect(ok.final_answer_contains).toEqual(["Hello"]);
    // No `must_include` field on the canonical shape — the [key: string]: unknown
    // escape hatch keeps extra unknown fields legal but typed access goes through
    // the canonical names.
    expect((ok as Record<string, unknown>).must_include).toBeUndefined();
  });

  it("curateItems posts expect.final_answer_contains so the server's non-empty guard passes", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      jsonResponse({ spec: { fixtures: [] }, meta: { revision: 2 } }),
    );
    vi.stubGlobal("fetch", fetchSpy);

    await evalApi.curateItems("demo-greetings", {
      from_run_id: "019e59cd-e4ae-7e63-920c-0e761c379f44",
      expect: { final_answer_contains: ["Hello"] },
    });

    expect(fetchSpy).toHaveBeenCalledTimes(1);
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe(`${BACKEND_URL}/v1/eval/datasets/demo-greetings/items`);
    const body = JSON.parse((init as RequestInit).body as string);
    expect(body.expect.final_answer_contains).toEqual(["Hello"]);
    expect(body.expect.must_include).toBeUndefined();
  });
});
