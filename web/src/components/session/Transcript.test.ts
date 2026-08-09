import { describe, expect, it } from "vitest";
import { userFacingRunError } from "./Transcript";

describe("userFacingRunError", () => {
  it("turns package capability failures into an actionable Environment message", () => {
    expect(userFacingRunError(
      "sandbox error: package requirements requested but backend cannot provision packages",
      true,
    )).toContain("编辑 Environment 移除 Package");
  });

  it("turns a stalled durable dispatch into a retry and worker recovery message", () => {
    expect(userFacingRunError(
      "runtime execution failed: durable run did not settle; dispatch pool never drove it to completion",
      false,
    )).toContain("Retry once");
  });
});
