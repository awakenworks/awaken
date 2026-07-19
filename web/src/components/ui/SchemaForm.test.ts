import { describe, expect, it } from "vitest";
import { stringControlForSchema } from "./SchemaForm";

describe("stringControlForSchema", () => {
  it("uses a multiline editor for prompt-shaped schema fields", () => {
    expect(stringControlForSchema({ type: ["string", "null"], format: "textarea" })).toBe("textarea");
  });

  it("keeps ordinary strings compact", () => {
    expect(stringControlForSchema({ type: "string" })).toBe("input");
  });
});
