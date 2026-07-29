import { describe, expect, it } from "vitest";
import {
  APPLICATION_TOKEN_CURL,
  FRONTEND_AI_SDK,
  MANAGED_CURL,
  PROTOCOLS,
} from "./protocols";

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

  it("separates service and application credentials", () => {
    expect(PROTOCOLS.filter((protocol) => protocol.token === "application").map((protocol) => protocol.id))
      .toEqual(["ai-sdk", "ag-ui"]);
    expect(PROTOCOLS.filter((protocol) => protocol.token === "service").map((protocol) => protocol.id))
      .toEqual(["managed", "a2a"]);
  });

  it("documents the complete backend exchange and frontend AI SDK wiring", () => {
    expect(APPLICATION_TOKEN_CURL).toContain("/v1/application-access-tokens");
    expect(APPLICATION_TOKEN_CURL).toContain("$AWAKEN_API_KEY");
    expect(FRONTEND_AI_SDK).toContain("DefaultChatTransport");
    expect(FRONTEND_AI_SDK).toContain("Bearer ${access_token}");
    expect(MANAGED_CURL).toContain("/v1/sessions");
    expect(MANAGED_CURL).toContain("$AWAKEN_API_KEY");
  });

});
