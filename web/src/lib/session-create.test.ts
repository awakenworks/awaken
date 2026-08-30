import { describe, expect, it } from "vitest";
import { buildSessionCreateRequest } from "./session-create";

describe("Session create request", () => {
  it("keeps the official top-level boundary and nests one-off MCP overrides under the Agent", () => {
    // Cause/effect decision table:
    // no complete MCP row -> plain Agent id; complete row -> tagged override;
    // incomplete rows -> omitted; retired top-level mcp_servers -> impossible.
    expect(buildSessionCreateRequest({
      agent: "agent-a", environmentId: "env-a", title: " ", vaultIds: [], mcpServers: [],
    })).toEqual({
      agent: "agent-a", environment_id: "env-a", title: undefined, vault_ids: [],
    });

    const request = buildSessionCreateRequest({
      agent: "agent-a",
      environmentId: "env-a",
      title: " Customer review ",
      vaultIds: ["vault-a"],
      mcpServers: [
        { name: " docs ", url: " https://mcp.example " },
        { name: "incomplete", url: "" },
      ],
    });
    expect(request).toEqual({
      agent: {
        id: "agent-a",
        type: "agent_with_overrides",
        mcp_servers: [{ type: "url", name: "docs", url: "https://mcp.example" }],
      },
      environment_id: "env-a",
      title: "Customer review",
      vault_ids: ["vault-a"],
    });
    expect(request).not.toHaveProperty("mcp_servers");
  });
});
