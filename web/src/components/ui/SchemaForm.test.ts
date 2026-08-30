import { describe, expect, it } from "vitest";
import { localizeSchema, stringControlForSchema } from "./SchemaForm";

describe("stringControlForSchema", () => {
  it("uses a multiline editor for prompt-shaped schema fields", () => {
    expect(stringControlForSchema({ type: ["string", "null"], format: "textarea" })).toBe("textarea");
  });

  it("keeps ordinary strings compact", () => {
    expect(stringControlForSchema({ type: "string" })).toBe("input");
  });

  it("localizes discovered behavior schema copy without changing property keys", () => {
    const schema = localizeSchema({
      type: "object",
      properties: {
        keep_last: {
          type: "integer",
          description: "Keep this many most-recent messages verbatim; the summary covers the rest.",
        },
      },
    }, "zh");
    expect(schema.properties?.keep_last?.title).toBe("原样保留消息数");
    expect(schema.properties?.keep_last?.description).toBe("原样保留最近这些消息，其余内容由摘要覆盖。");
    expect(localizeSchema(schema, "en")).toBe(schema);
  });
});
