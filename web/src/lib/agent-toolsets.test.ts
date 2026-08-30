import { describe, expect, it } from "vitest";
import type {
  AgentConfig,
  AgentManagedToolset,
  AgentMcpServer,
  AgentTool,
  AgentToolsetMemberCap,
} from "./api/types";
import {
  controlledModificationMemberNames,
  mcpDefaultConfig,
  mcpIntegrationsValid,
  mcpToolsetPolicySummary,
  reconcileMcpToolsets,
  removeMcpIntegration,
  renameMcpIntegrationReferences,
  renameMcpToolset,
  selectedAgentToolIds,
  withSelectedAgentTools,
} from "./agent-toolsets";

const servers: AgentMcpServer[] = [
  { name: "docs", url: "https://docs.test/mcp" },
  { type: "sandbox_stdio", name: "browser", command: "playwright-mcp" },
];

describe("MCP Integration aggregate", () => {
  it("uses the backend-advertised MCP default and falls back fail-closed", () => {
    // Decision rules: D1 advertised MCP policy -> copy that exact default;
    // D2 absent capability -> enabled with always_ask, never always_allow.
    expect(mcpDefaultConfig(undefined)?.permission_policy?.type).toBe("always_ask");
    expect(mcpDefaultConfig([{
      type: "mcp_toolset",
      source_kind: "mcp",
      dynamic_members: true,
      default_config: { enabled: false, permission_policy: { type: "always_allow" } },
      member_configurable_fields: ["enabled", "permission_policy"],
    }])).toEqual({ enabled: false, permission_policy: { type: "always_allow" } });
  });

  it("creates exactly one policy per server and preserves existing overrides", () => {
    // Cause/effect rules: R1 paired server+policy -> preserve the existing
    // policy; R2 server without policy -> create one from the default; no
    // unrelated ToolSet is fabricated.
    const tools: AgentTool[] = [{
      type: "mcp_toolset",
      mcp_server_name: "docs",
      configs: [{ name: "search", permission_policy: { type: "always_allow" } }],
    }];
    const result = reconcileMcpToolsets(tools, servers, mcpDefaultConfig(undefined));
    expect(result.filter((tool) => typeof tool === "object" && tool.type === "mcp_toolset")).toEqual([
      tools[0],
      expect.objectContaining({
        mcp_server_name: "browser",
        default_config: {
          enabled: true,
          permission_policy: { type: "always_ask" },
        },
      }),
    ]);
  });

  it("reconciles MCP policy without changing Agent, custom, or Runtime-only configuration", () => {
    // Cause/effect cross-table:
    // | rule | input branch                         | effect                 |
    // | X1   | Agent ToolSet + Runtime-only member | same object and bytes  |
    // | X2   | custom client tool                  | same object and bytes  |
    // | X3   | exact id                            | same value/order        |
    // | X4   | paired MCP ToolSet                  | same policy object      |
    // | X5   | new MCP server                      | one new default policy  |
    const agent: AgentTool = {
      type: "agent_toolset_20260401",
      default_config: { enabled: false, permission_policy: { type: "always_allow" } },
      configs: [{
        name: "agent_run",
        enabled: true,
        permission_policy: { type: "always_ask" },
      }],
    };
    const custom: AgentTool = {
      type: "custom",
      name: "local_review",
      description: "review in the trusted client",
      input_schema: { type: "object", required: ["patch_id"] },
    };
    const docs: AgentTool = {
      type: "mcp_toolset",
      mcp_server_name: "docs",
      configs: [{ name: "search", enabled: false }],
    };
    const result = reconcileMcpToolsets(
      ["custom_static", agent, custom, docs],
      servers,
      mcpDefaultConfig(undefined),
    );

    expect(result.slice(0, 3)).toEqual(["custom_static", agent, custom]);
    expect(result[1]).toBe(agent);
    expect(result[2]).toBe(custom);
    expect(result[3]).toBe(docs);
    expect(result[4]).toEqual(expect.objectContaining({
      type: "mcp_toolset",
      mcp_server_name: "browser",
    }));
  });

  it("projects the policy beside its MCP server and fails closed for legacy drafts", () => {
    // Rules: P1 absent policy -> enabled/ask/zero overrides; P2 persisted
    // explicit fields -> project those values and the exact override count.
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
    // State-transition table: N1 rename -> server-scoped ToolSet, override,
    // recovery and exposure ids all move together; N2 remove -> those same
    // MCP-scoped values disappear while non-MCP recovery/exposure survives.
    const tools = renameMcpToolset(
      reconcileMcpToolsets([], [servers[0]], mcpDefaultConfig(undefined)),
      "docs",
      "knowledge",
    );
    expect(tools).toEqual([expect.objectContaining({ mcp_server_name: "knowledge" })]);
    const config = {
      mcp_servers: [servers[0]],
      tools: reconcileMcpToolsets([], [servers[0]], mcpDefaultConfig(undefined)),
      tool_overrides: [{ target: "mcp__docs__search", alias: "lookup" }],
      recovery_policies: {
        mcp__docs__search: { mode: "never_replay" },
        read: { mode: "replay_safe" },
      },
      tool_exposure: { rules: [
        { selector: { kind: "prefix", value: "mcp__docs__" }, exposure: "on_demand" },
        { selector: { kind: "exact", value: "read" }, exposure: "eager" },
      ] },
    } as unknown as AgentConfig;
    expect(renameMcpIntegrationReferences(config, "docs", "knowledge")).toEqual(expect.objectContaining({
      tools: [expect.objectContaining({ mcp_server_name: "knowledge" })],
      tool_overrides: [expect.objectContaining({ target: "mcp__knowledge__search" })],
      recovery_policies: expect.objectContaining({
        mcp__knowledge__search: { mode: "never_replay" },
      }),
      tool_exposure: expect.objectContaining({ rules: expect.arrayContaining([
        expect.objectContaining({
          selector: { kind: "prefix", value: "mcp__knowledge__" },
        }),
      ]) }),
    }));
    expect(removeMcpIntegration(config, "docs")).toEqual(expect.objectContaining({
      mcp_servers: [],
      tools: [],
      tool_overrides: [],
      recovery_policies: { read: { mode: "replay_safe" } },
      tool_exposure: {
        rules: [{ selector: { kind: "exact", value: "read" }, exposure: "eager" }],
      },
    }));
  });

  it("rejects every incomplete or non-bijective effective configuration", () => {
    // Validity decision table: V1 unique non-empty server with one policy ->
    // valid; V2 missing policy, V3 blank name/transport, or V4 duplicate name
    // -> invalid before publication.
    const base = {
      mcp_servers: [servers[0]],
      tools: reconcileMcpToolsets([], [servers[0]], mcpDefaultConfig(undefined)),
    } as unknown as AgentConfig;
    expect(mcpIntegrationsValid(base)).toBe(true);
    expect(mcpIntegrationsValid({ ...base, tools: [] })).toBe(false);
    expect(mcpIntegrationsValid({
      ...base,
      mcp_servers: [{ ...servers[0], name: "" }],
    })).toBe(false);
    expect(mcpIntegrationsValid({
      ...base,
      mcp_servers: [{ name: "docs", url: "" }],
    })).toBe(false);
    expect(mcpIntegrationsValid({
      ...base,
      mcp_servers: [servers[0], servers[0]],
    })).toBe(false);
  });
});

function member(
  name: string,
  controlledModification: boolean,
): AgentToolsetMemberCap {
  return {
    name,
    available: true,
    controlled_modification: controlledModification,
    configurable_fields: ["enabled", "permission_policy"],
  };
}

const members = [
  member("bash", true),
  member("read", false),
  member("write", true),
  member("edit", true),
  member("glob", false),
  member("grep", false),
  member("web_fetch", false),
  member("web_search", false),
];
const advertisedAllow = { type: "always_allow" } as const;

describe("canonical Agent toolset authoring", () => {
  it("derives the controlled description roster from capability members", () => {
    // Cause/effect rule U1: capability members and their controlled flags are
    // the only input; the UI roster contains exactly flagged names in wire
    // order. Adding/removing a capability therefore cannot drift from authoring.
    expect(controlledModificationMemberNames([
      member("shell", true),
      member("read", false),
      member("replace", true),
    ])).toEqual(["shell", "replace"]);
  });

  it("replaces official selection without creating a parallel exact-id path", () => {
    // Causes: C1 a server-preset Toolset contains explicit asks; C2 the picker
    // changes the selected official members; C3 a newly selected member has no
    // explicit policy. Effects: E1 one Toolset remains; E2 official strings are
    // absent; E3 the server-authored Bash ask survives; E4 fresh Edit uses the
    // advertised ordinary allow default; E5 repeating the projection is stable.
    // Rules: S1=C1+C2+C3 => E1-E4; S2=replay S1 => E5.
    // The server-authored preset result is input here; Web owns only picker
    // selection and must preserve, not recreate, its permission decisions.
    const controlled: AgentTool[] = [{
      type: "agent_toolset_20260401",
      default_config: { enabled: false, permission_policy: { type: "always_allow" } },
      configs: [
        { name: "read", enabled: true, permission_policy: { type: "always_allow" } },
        { name: "write", enabled: true, permission_policy: { type: "always_ask" } },
        { name: "bash", enabled: true, permission_policy: { type: "always_ask" } },
      ],
    }];
    const selection = ["bash", "read", "edit", "custom_static"];
    const first = withSelectedAgentTools(controlled, selection, members, advertisedAllow);
    const second = withSelectedAgentTools(first, selection, members, advertisedAllow);
    expect(second).toEqual(first);
    expect(selectedAgentToolIds(first, members)).toEqual(selection);
    expect(first.filter(
      (tool) => typeof tool !== "string" && tool.type === "agent_toolset_20260401",
    )).toHaveLength(1);
    expect(first).not.toContain("read");
    expect(first).not.toContain("edit");
    const policies = Object.fromEntries((first.find(
      (tool): tool is AgentManagedToolset =>
        typeof tool === "object" && tool.type === "agent_toolset_20260401",
    )?.configs ?? []).map((entry) => [entry.name, entry.permission_policy?.type]));
    expect(policies).toEqual(expect.objectContaining({
      bash: "always_ask",
      edit: "always_allow",
    }));
  });

  it("keeps an ordinary save on the advertised allow default", () => {
    // Cause/effect graph: C1 Web selects fresh official members; C2 capability
    // metadata projects the Agent Toolset default allow; C3 no transient server
    // preset was requested. Effects: E1 Bash/read/write are all allow; E2 the Web
    // projection does not infer asks from controlled-modification flags. Rule
    // O1=C1+C2+C3=>E1+E2. The preceding S1 rule covers the paired server-preset
    // outcome: an explicit ask returned by that owner remains ask.
    const projected = withSelectedAgentTools(
      [],
      ["bash", "read", "write"],
      members,
      advertisedAllow,
    );
    const toolset = projected.find(
      (tool): tool is AgentManagedToolset =>
        typeof tool === "object" && tool.type === "agent_toolset_20260401",
    );
    expect(toolset).toBeDefined();
    const policies = Object.fromEntries(
      (toolset?.configs ?? []).map((entry) => [entry.name, entry.permission_policy?.type]),
    );
    expect(policies).toEqual(expect.objectContaining({
      bash: "always_allow",
      read: "always_allow",
      write: "always_allow",
    }));
    const alternateCapability = withSelectedAgentTools(
      [],
      ["read"],
      members,
      { type: "always_ask" },
    ).find(
      (tool): tool is AgentManagedToolset =>
        typeof tool === "object" && tool.type === "agent_toolset_20260401",
    );
    expect(alternateCapability?.configs?.[0]?.permission_policy?.type).toBe("always_ask");
  });
});
