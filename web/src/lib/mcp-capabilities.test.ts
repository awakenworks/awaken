import { describe, expect, it } from "vitest";
import { MCP_CAPABILITY_COVERAGE } from "./mcp-capabilities";

describe("MCP product capability coverage", () => {
  it("does not mistake background execution or task-shaped DTOs for MCP Tasks", () => {
    const tasks = MCP_CAPABILITY_COVERAGE.find((entry) => entry.id === "tasks");
    expect(tasks).toEqual({ id: "tasks", integration: "unavailable", exportedServer: "unavailable" });
  });

  it("keeps transport progress distinct from Session-visible integration progress", () => {
    const progress = MCP_CAPABILITY_COVERAGE.find((entry) => entry.id === "progress");
    expect(progress).toEqual({ id: "progress", integration: "conditional", exportedServer: "supported" });
  });
});
