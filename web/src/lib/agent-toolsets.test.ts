import { describe, expect, it } from "vitest";
import type { AgentConfig, AgentMcpServer, AgentTool } from "./api/types";
import {
  mcpDefaultConfig,
  mcpIntegrationsValid,
  mcpToolsetPolicySummary,
  reconcileMcpToolsets,
  removeMcpIntegration,
  renameMcpIntegrationReferences,
  renameMcpToolset,
} from "./agent-toolsets";

const servers: AgentMcpServer[] = [
  { name: "docs", url: "https://docs.test/mcp" },
  { type: "sandbox_stdio", name: "browser", command: "playwright-mcp" },
];

describe("MCP Integration aggregate", () => {
  it("uses the backend-advertised MCP default and falls back fail-closed", () => {
    expect(mcpDefaultConfig(undefined)?.permission_policy?.type).toBe("always_ask");
    expect(mcpDefaultConfig([{
      type: "mcp_toolset", source_kind: "mcp", dynamic_members: true,
      default_config: { enabled: false, permission_policy: { type: "always_allow" } },
    }])).toEqual({ enabled: false, permission_policy: { type: "always_allow" } });
  });

  it("creates exactly one policy per server and preserves existing overrides", () => {
    const tools: AgentTool[] = [{
      type: "mcp_toolset", mcp_server_name: "docs",
      configs: [{ name: "search", permission_policy: { type: "always_allow" } }],
    }];
    const result = reconcileMcpToolsets(tools, servers, mcpDefaultConfig(undefined));
    expect(result.filter((tool) => typeof tool === "object" && tool.type === "mcp_toolset")).toEqual([
      tools[0],
      expect.objectContaining({ mcp_server_name: "browser", default_config: { enabled: true, permission_policy: { type: "always_ask" } } }),
    ]);
  });

  it("projects the policy beside its MCP server and fails closed for legacy drafts", () => {
    expect(mcpToolsetPolicySummary([], "docs")).toEqual({
      enabled: true,
      permission: "always_ask",
      namedOverrides: 0,
    });
    expect(mcpToolsetPolicySummary([{
      type: "mcp_toolset",
      mcp_server_name: "docs",
      default_config: { enabled: false, permission_policy: { type: "always_allow" } },
      configs: [{ name: "search" }, { name: "fetch" }],
    }], "docs")).toEqual({
      enabled: false,
      permission: "always_allow",
      namedOverrides: 2,
    });
  });

  it("renames and removes the whole integration without leaving policy debris", () => {
    const tools = renameMcpToolset(reconcileMcpToolsets([], [servers[0]], mcpDefaultConfig(undefined)), "docs", "knowledge");
    expect(tools).toEqual([expect.objectContaining({ mcp_server_name: "knowledge" })]);
    const config = {
      mcp_servers: [servers[0]], tools: reconcileMcpToolsets([], [servers[0]], mcpDefaultConfig(undefined)),
      tool_overrides: [{ target: "mcp__docs__search", alias: "lookup" }],
      recovery_policies: { mcp__docs__search: { mode: "never_replay" }, read: { mode: "replay_safe" } },
      tool_exposure: { rules: [
        { selector: { kind: "prefix", value: "mcp__docs__" }, exposure: "on_demand" },
        { selector: { kind: "exact", value: "read" }, exposure: "eager" },
      ] },
    } as unknown as AgentConfig;
    expect(renameMcpIntegrationReferences(config, "docs", "knowledge")).toEqual(expect.objectContaining({
      tools: [expect.objectContaining({ mcp_server_name: "knowledge" })],
      tool_overrides: [expect.objectContaining({ target: "mcp__knowledge__search" })],
      recovery_policies: expect.objectContaining({ mcp__knowledge__search: { mode: "never_replay" } }),
      tool_exposure: expect.objectContaining({ rules: expect.arrayContaining([
        expect.objectContaining({ selector: { kind: "prefix", value: "mcp__knowledge__" } }),
      ]) }),
    }));
    expect(removeMcpIntegration(config, "docs")).toEqual(expect.objectContaining({
      mcp_servers: [], tools: [], tool_overrides: [], recovery_policies: { read: { mode: "replay_safe" } },
      tool_exposure: { rules: [{ selector: { kind: "exact", value: "read" }, exposure: "eager" }] },
    }));
  });

  it("rejects every incomplete or non-bijective effective configuration", () => {
    const base = { mcp_servers: [servers[0]], tools: reconcileMcpToolsets([], [servers[0]], mcpDefaultConfig(undefined)) } as unknown as AgentConfig;
    expect(mcpIntegrationsValid(base)).toBe(true);
    expect(mcpIntegrationsValid({ ...base, tools: [] })).toBe(false);
    expect(mcpIntegrationsValid({ ...base, mcp_servers: [{ ...servers[0], name: "" }] })).toBe(false);
    expect(mcpIntegrationsValid({ ...base, mcp_servers: [{ name: "docs", url: "" }] })).toBe(false);
    expect(mcpIntegrationsValid({ ...base, mcp_servers: [servers[0], servers[0]] })).toBe(false);
  });
});
