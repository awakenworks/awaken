// MCP egress proof: the production Awaken binary exposes an explicit, bearer-
// protected tool set over Streamable HTTP.
import { BACKEND, requireOk } from "../support/control-plane.mjs";

const TOKEN = process.env.AWAKEN_RECORD_MCP_TOKEN ?? "";

export async function run({ page, goto, checkpoint, expect }) {
  if (!TOKEN) throw new Error("18-mcp-server-export requires AWAKEN_RECORD_MCP_TOKEN matching the all-in-one config's mcp_bearer_token");
  await goto("/w/default/protocols");
  const card = page.locator(".card").filter({ hasText: "MCP Server" });
  await expect(card).toBeVisible();

  const headers = {
    authorization: `Bearer ${TOKEN}`,
    accept: "application/json, text/event-stream",
    "content-type": "application/json",
  };
  const initialized = await page.request.post(`${BACKEND}/v1/mcp`, {
    headers,
    data: {
      jsonrpc: "2.0", id: 1, method: "initialize",
      params: { protocolVersion: "2025-06-18" },
    },
  });
  await requireOk(initialized, "MCP initialize");
  const sessionId = initialized.headers()["mcp-session-id"];
  expect(sessionId).toBeTruthy();

  const toolsResponse = await page.request.post(`${BACKEND}/v1/mcp`, {
    headers: { ...headers, "mcp-session-id": sessionId },
    data: { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} },
  });
  await requireOk(toolsResponse, "MCP tools/list");
  const tools = await toolsResponse.json();
  await checkpoint("the authenticated MCP client receives only the explicit Awaken exports", async () => {
    const names = tools.result.tools.map((tool) => tool.name);
    expect(names).toEqual(expect.arrayContaining([
      "admin_get_platform_capabilities",
      "admin_draft_agent",
      "admin_validate_agent",
      "admin_explain_console",
    ]));
    expect(names.some((name) => name === "bash" || name.startsWith("mcp__"))).toBeFalsy();
    await expect(card).toContainText("/v1/mcp");
  });
}
