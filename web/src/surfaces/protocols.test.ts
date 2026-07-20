import { describe, expect, it } from "vitest";
import { PROTOCOLS } from "./protocols";

describe("built-in protocol guide", () => {
  it("covers every production adapter with a unique callable endpoint", () => {
    expect(PROTOCOLS.map((protocol) => protocol.id)).toEqual([
      "managed", "ai-sdk", "ag-ui", "a2a", "mcp",
    ]);
    expect(new Set(PROTOCOLS.map((protocol) => protocol.endpoint)).size).toBe(PROTOCOLS.length);
  });

  it("marks MCP as dedicated-token gated", () => {
    expect(PROTOCOLS.find((protocol) => protocol.id === "mcp")?.token).toBe("dedicated");
  });
});
