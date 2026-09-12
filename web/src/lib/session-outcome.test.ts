import { describe, expect, it } from "vitest";
import type { Session } from "./api/types";
import {
  OUTCOME_ITERATIONS_DEFAULT,
  OUTCOME_ITERATIONS_MAX,
  OUTCOME_RUBRIC_MAX_CHARS,
  activeOutcome,
  clampOutcomeIterations,
  defineOutcomeEvent,
  emptyOutcomeDraft,
  outcomeDraftComplete,
} from "./session-outcome";

function evaluation(overrides: Record<string, unknown>) {
  return {
    completed_at: null,
    description: "Ship the brief",
    explanation: null,
    iteration: 0,
    outcome_id: "otcm_1",
    result: "running",
    ...overrides,
  };
}

function sessionWith(outcomes: ReturnType<typeof evaluation>[]): Session {
  return { outcome_evaluations: outcomes } as unknown as Session;
}

describe("outcome authoring", () => {
  it("keeps the iteration budget inside the range the wire contract states", () => {
    // Cause/effect: R1 in-range -> unchanged; R2 below/above -> the boundary;
    // R3 fractional -> truncated toward the operator's typed digits; R4 not a
    // number (an emptied numeric input) -> the documented default, never NaN.
    expect(clampOutcomeIterations(5)).toBe(5);
    expect(clampOutcomeIterations(0)).toBe(1);
    expect(clampOutcomeIterations(999)).toBe(OUTCOME_ITERATIONS_MAX);
    expect(clampOutcomeIterations(4.8)).toBe(4);
    expect(clampOutcomeIterations(Number.NaN)).toBe(OUTCOME_ITERATIONS_DEFAULT);
  });

  it("refuses a draft the runtime would reject or grade against nothing", () => {
    expect(outcomeDraftComplete(emptyOutcomeDraft())).toBe(false);
    expect(outcomeDraftComplete({ description: "  ", rubric: "r", maxIterations: 3 })).toBe(false);
    expect(outcomeDraftComplete({ description: "d", rubric: "   ", maxIterations: 3 })).toBe(false);
    expect(outcomeDraftComplete({ description: "d", rubric: "r", maxIterations: 3 })).toBe(true);
    expect(outcomeDraftComplete({
      description: "d",
      rubric: "x".repeat(OUTCOME_RUBRIC_MAX_CHARS + 1),
      maxIterations: 3,
    })).toBe(false);
  });

  it("builds one exact user.define_outcome event with a text rubric", () => {
    expect(defineOutcomeEvent({
      description: "  Ship the brief  ",
      rubric: "  - cites every claim  ",
      maxIterations: 40,
    })).toEqual({
      type: "user.define_outcome",
      description: "Ship the brief",
      rubric: { type: "text", content: "- cites every claim" },
      max_iterations: OUTCOME_ITERATIONS_MAX,
    });
  });

  it("treats an ungraded evaluation as the one active Outcome", () => {
    // The aggregate drives one Outcome at a time and back-pressures a second
    // definition into a pending batch, so "active" must key on the absence of a
    // completion timestamp rather than on any result string.
    expect(activeOutcome(undefined)).toBeUndefined();
    expect(activeOutcome(sessionWith([]))).toBeUndefined();
    expect(activeOutcome(sessionWith([
      evaluation({ outcome_id: "done", completed_at: "2026-09-12T00:00:00Z", result: "satisfied" }),
    ]))).toBeUndefined();
    expect(activeOutcome(sessionWith([
      evaluation({ outcome_id: "done", completed_at: "2026-09-12T00:00:00Z", result: "satisfied" }),
      evaluation({ outcome_id: "live" }),
    ]))).toMatchObject({ outcome_id: "live" });
  });
});
