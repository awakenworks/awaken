export type McpSupport = "supported" | "conditional" | "unavailable";

export interface McpCapabilityCoverage {
  id: string;
  integration: McpSupport;
  exportedServer: McpSupport;
}

/**
 * Product-level MCP support, not merely wire DTO presence. Conditional means a
 * transport primitive exists but the Agent/Session user journey is narrower.
 */
export const MCP_CAPABILITY_COVERAGE: McpCapabilityCoverage[] = [
  { id: "tools", integration: "supported", exportedServer: "supported" },
  { id: "transport", integration: "supported", exportedServer: "supported" },
  { id: "catalog_updates", integration: "supported", exportedServer: "supported" },
  { id: "progress", integration: "conditional", exportedServer: "supported" },
  { id: "cancellation", integration: "conditional", exportedServer: "supported" },
  { id: "prompts", integration: "conditional", exportedServer: "unavailable" },
  { id: "resources", integration: "conditional", exportedServer: "unavailable" },
  { id: "sampling", integration: "unavailable", exportedServer: "unavailable" },
  { id: "elicitation_roots", integration: "unavailable", exportedServer: "unavailable" },
  { id: "tasks", integration: "unavailable", exportedServer: "unavailable" },
];
