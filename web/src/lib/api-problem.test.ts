import { describe, expect, it } from "vitest";
import { ApiClientError } from "./api/client";
import { presentApiProblem } from "./api-problem";

describe("API problem cause/effect classification", () => {
  // Decision table (FMECA generic UI error S7/O8/D8=448):
  // R1 401 -> authenticate; R2 403 -> authorize with server action/scope detail;
  // R3 409 -> refresh the exact conflicting fact before explicit retry;
  // R4 503 -> retry the named dependency; R5 every rule preserves stable code,
  // detail and correlation ID. Unknown status is inspect, never a blind retry.
  it.each([
    [401, "authenticate"],
    [403, "authorize"],
    [409, "refresh_conflict"],
    [503, "retry_dependency"],
    [500, "inspect"],
  ] as const)("maps HTTP %s to %s without losing evidence", (status, action) => {
    const problem = presentApiProblem(new ApiClientError(
      status,
      `problem_${status}`,
      `detail ${status}`,
      `request-${status}`,
    ));
    expect(problem).toEqual({
      action,
      code: `problem_${status}`,
      detail: `detail ${status}`,
      requestId: `request-${status}`,
    });
  });
});
