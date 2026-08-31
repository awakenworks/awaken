import { describe, expect, it } from "vitest";
import {
  APPLICATION_TOKEN_CURL,
  FRONTEND_AI_SDK,
  MANAGED_SDK,
  managedSdkExample,
  PROTOCOL_HELP,
  PROTOCOL_EXTENSIONS,
  PROTOCOLS,
  protocolDocsUrl,
  managedCredentialLabel,
} from "./protocols";

describe("built-in protocol guide", () => {
  it("covers every production adapter with a unique callable endpoint", () => {
    expect(PROTOCOLS.map((protocol) => protocol.id)).toEqual([
      "managed", "ai-sdk", "ag-ui", "a2a", "mcp",
    ]);
    expect(new Set(PROTOCOLS.map((protocol) => protocol.endpoint)).size).toBe(PROTOCOLS.length);
    expect(new Set(PROTOCOLS.map((protocol) => protocol.docs)).size).toBe(PROTOCOLS.length);
    expect(PROTOCOLS.every((protocol) => protocol.useWhen[0].length > 20 && protocol.useWhen[1].length > 10)).toBe(true);
  });

  it("links every protocol to its localized Awaken Agents documentation", () => {
    expect(protocolDocsUrl("en", "managed-agents"))
      .toBe("https://awakenworks.com/docs/agents/protocols/managed-agents/");
    expect(protocolDocsUrl("zh", "managed-agents"))
      .toBe("https://awakenworks.com/zh/docs/agents/protocols/managed-agents/");
    expect(PROTOCOL_EXTENSIONS.map((extension) => extension.docs)).toEqual(["acp", "live-inbox"]);
    for (const extension of PROTOCOL_EXTENSIONS) {
      expect(protocolDocsUrl("en", extension.docs))
        .toBe(`https://awakenworks.com/docs/agents/protocols/${extension.docs}/`);
      expect(extension.useWhen[0].length).toBeGreaterThan(20);
      expect(extension.guidance[0].length).toBeGreaterThan(20);
    }
  });

  it("does not send no-login users to unavailable service-key management", () => {
    expect(managedCredentialLabel(false, "no-login", "en")).toBe("No key required in local no-login mode");
    expect(managedCredentialLabel(false, "no-login", "zh")).toBe("本地 no-login 模式无需 Key");
    expect(managedCredentialLabel(true, "self-managed", "en")).toBe("Workspace service API key");
    expect(managedCredentialLabel(false, "self-managed", "en")).toBe("Service API keys unavailable in this deployment");
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

  it("keeps connection guidance with every protocol", () => {
    expect(Object.keys(PROTOCOL_HELP)).toEqual(PROTOCOLS.map((protocol) => protocol.id));
    for (const protocol of PROTOCOLS) {
      const help = PROTOCOL_HELP[protocol.id];
      expect(help.steps).toHaveLength(3);
      expect(help.credential.length).toBeGreaterThan(0);
      expect("example" in help || "href" in help).toBe(true);
    }
    expect(PROTOCOL_HELP.a2a.href).toBe("a2a-servers");
    expect(PROTOCOL_HELP.mcp.href).toBe("mcp");
    expect(PROTOCOL_HELP.managed.href).toBe("access");
    expect(PROTOCOL_HELP.managed.steps.join(" ")).toContain("one-time secret");
    expect(PROTOCOL_HELP.managed.steps.join(" ")).toContain("Never expose this key");
  });

  it("documents the complete backend exchange and frontend AI SDK wiring", () => {
    expect(APPLICATION_TOKEN_CURL).toContain("/v1/application-access-tokens");
    expect(APPLICATION_TOKEN_CURL).toContain("$AWAKEN_URL");
    expect(APPLICATION_TOKEN_CURL).toContain("$AWAKEN_API_KEY");
    expect(APPLICATION_TOKEN_CURL).toContain('"protocols": ["ai-sdk"]');
    expect(APPLICATION_TOKEN_CURL).toContain('"managed_session_id": "sesn_123"');
    expect(APPLICATION_TOKEN_CURL).not.toContain("authority_id");
    expect(APPLICATION_TOKEN_CURL).not.toContain("application_scope");
    expect(APPLICATION_TOKEN_CURL).not.toContain("actor_key");
    expect(APPLICATION_TOKEN_CURL).not.toContain("thread_namespace");
    expect(FRONTEND_AI_SDK).toContain("DefaultChatTransport");
    expect(FRONTEND_AI_SDK).toContain("Bearer ${access_token}");
    expect(FRONTEND_AI_SDK).toContain("thread_id: threadId");
    expect(MANAGED_SDK).toContain('from "@anthropic-ai/sdk"');
    expect(MANAGED_SDK).toContain("npm install @anthropic-ai/sdk");
    expect(MANAGED_SDK).toContain("baseURL: process.env.AWAKEN_BASE_URL");
    expect(MANAGED_SDK).toContain("apiKey: process.env.AWAKEN_API_KEY");
    expect(MANAGED_SDK).toContain('"managed-agents-2026-04-01"');
    expect(MANAGED_SDK).toContain("beta.sessions.create");
    expect(MANAGED_SDK).toContain("beta.sessions.events.send");
    expect(MANAGED_SDK).toContain("AWAKEN_AGENT_ID");
    expect(MANAGED_SDK).toContain("AWAKEN_ENVIRONMENT_ID");
    expect(MANAGED_SDK).toContain("Open in Console");
    expect(`${APPLICATION_TOKEN_CURL}${MANAGED_SDK}`).not.toContain("localhost:8080");
  });

  it("personalizes the one Managed SDK template without changing its credential boundary", () => {
    // Cause/effect decision table: R1 absent context -> environment-variable
    // placeholders; R2 Session Agent + Environment + Workspace -> safely
    // quoted exact coordinates and an encoded Console path. Both rules retain
    // backend-only API key/base URL variables and the same SDK lifecycle.
    const contextual = managedSdkExample({
      agentId: 'reviewer "one"',
      environmentId: "env/one",
      workspaceId: "team space",
    });
    expect(contextual).toContain('agent: "reviewer \\"one\\""');
    expect(contextual).toContain('environment_id: "env/one"');
    expect(contextual).toContain("/w/team%20space/sessions/${session.id}");
    expect(contextual).toContain("apiKey: process.env.AWAKEN_API_KEY");
    expect(contextual).toContain("baseURL: process.env.AWAKEN_BASE_URL");
    expect(contextual).not.toContain("AWAKEN_AGENT_ID");
    expect(contextual).not.toContain("AWAKEN_ENVIRONMENT_ID");
  });

});
