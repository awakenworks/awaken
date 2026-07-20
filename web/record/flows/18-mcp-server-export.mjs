// MCP egress proof: the production Awaken binary exposes an explicit, bearer-
// protected tool set over Streamable HTTP.

const TOKEN = process.env.AWAKEN_RECORD_MCP_TOKEN ?? "";
const BACKEND = "http://127.0.0.1:38080";

export const story = {
  promise: "Let any MCP-capable client use Awaken's governed platform tools without embedding the console or a proprietary SDK.",
  effect: "A bearer-authenticated MCP handshake returns the explicit Awaken management tool catalog from /v1/mcp.",
  aha: "Awaken consumes MCP for Agent tools and serves MCP for platform control—the boundary works in both directions.",
  loyalty: "A standards-based tool boundary protects client integrations as frameworks and desktop hosts change.",
  satisfaction: "A visible endpoint, explicit enable switch, and fail-closed bearer remove setup and security ambiguity.",
  advocacy: "One product acting as both MCP client and server is a concise, memorable platform capability.",
};

export async function run({ page, goto, intro, beat, clearCaption, checkpoint, aha, expect, wait }) {
  if (!TOKEN) throw new Error("18-mcp-server-export requires AWAKEN_RECORD_MCP_TOKEN matching the host's AWAKEN_MCP_BEARER_TOKEN");
  await goto("/w/default/protocols");
  await intro(
    "Expose governed platform capabilities to existing MCP clients without opening an unauthenticated control surface.",
    "The production binary mounts an explicit Streamable HTTP export only when a dedicated bearer is configured.",
  );
  const card = page.locator(".card").filter({ hasText: "MCP Server" });
  await beat("The route is opt-in and uses a dedicated bearer; without it, /v1/mcp is not mounted.", card, 3800);

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
  expect(initialized.ok()).toBeTruthy();
  const sessionId = initialized.headers()["mcp-session-id"];
  expect(sessionId).toBeTruthy();

  const toolsResponse = await page.request.post(`${BACKEND}/v1/mcp`, {
    headers: { ...headers, "mcp-session-id": sessionId },
    data: { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} },
  });
  expect(toolsResponse.ok()).toBeTruthy();
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
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
