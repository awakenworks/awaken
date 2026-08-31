import { describe, expect, it } from "vitest";
import { workspaceLabel } from "../app-state";
import type { ConfigCapabilitiesView } from "../api/types";
import {
  NAV,
  WORKSPACE_JOURNEY,
  navPath,
  protocolHelpPath,
  quickstartSessionPath,
  titleForPath,
  visibleNavigation,
} from "./paths";

function capabilities(
  byokEnabled: boolean,
  managedRuntime: boolean,
  accessManagement: boolean,
): ConfigCapabilitiesView {
  return {
    identity: { mode: "test", cloud_login_enabled: false, authenticated: true },
    models: {
      local_catalog_enabled: byokEnabled,
      byok_enabled: byokEnabled,
      cloud_models_enabled: !byokEnabled,
      profile_authoring_enabled: byokEnabled,
    },
    surfaces: {
      managed_runtime: managedRuntime,
      access_management: accessManagement,
    },
  };
}

describe("console information architecture", () => {
  it("keeps one source-of-truth route for every primary surface", () => {
    expect(NAV.map((item) => item.group)).toEqual([
      "workspace",
      "author",
      "author",
      "author",
      "author",
      "run",
      "run",
      "run",
      "run",
      "connect",
      "connect",
      "connect",
      "connect",
      "connect",
      "govern",
      "govern",
      "govern",
    ]);
    expect(NAV.find((item) => item.key === "files")).toMatchObject({
      group: "author",
      sectionLabel: "Resources",
    });
    expect(NAV.find((item) => item.key === "artifacts")).toMatchObject({
      group: "run",
      path: "/w/:ws/artifacts",
    });
    expect(NAV.find((item) => item.key === "mcp")).toMatchObject({
      group: "connect",
      path: "/w/:ws/mcp",
    });
    expect(NAV.find((item) => item.key === "webhooks")).toMatchObject({
      group: "connect",
      path: "/w/:ws/webhooks",
    });
    expect(NAV.some((item) => item.key === "credentials")).toBe(false);
  });

  it("resolves the added workspace paths and titles", () => {
    const artifacts = NAV.find((item) => item.key === "artifacts")!;
    expect(navPath(artifacts, "workspace-a")).toBe("/w/workspace-a/artifacts");
    expect(titleForPath("/w/workspace-a/mcp")).toEqual({
      scope: "workspace-a",
      title: "MCP overview",
    });
  });

  it("routes Quickstart into the canonical durable Session with presentational provenance", () => {
    // Cause/effect decision table: R1 safe Workspace + Session -> their
    // canonical detail route plus a non-authoritative source query; R2 empty
    // Workspace -> existing default scope; R3 reserved route characters ->
    // encoded coordinates. The query changes guidance only and never creates a
    // second Session route or onboarding state machine.
    expect(quickstartSessionPath("workspace-a", "sesn_123")).toBe(
      "/w/workspace-a/sessions/sesn_123?from=quickstart",
    );
    expect(quickstartSessionPath("", "sesn_123")).toBe(
      "/w/default/sessions/sesn_123?from=quickstart",
    );
    expect(quickstartSessionPath("team space", "session/1")).toBe(
      "/w/team%20space/sessions/session%2F1?from=quickstart",
    );
  });

  it("deep-links every protocol CTA through one encoded route owner", () => {
    // Cause/effect table: R1 explicit Workspace + protocol -> the matching
    // help fragment; R2 empty Workspace -> default scope; R3 reserved
    // characters -> encoded path/hash coordinates; R4 optional Agent and
    // Environment -> presentational query before the fragment; R5 blank
    // context -> omitted. All callers reuse this helper instead of copying URL
    // construction, and the query never owns execution state.
    expect(protocolHelpPath("workspace-a", "managed")).toBe(
      "/w/workspace-a/protocols#protocol-managed",
    );
    expect(protocolHelpPath("workspace-a", "acp")).toBe(
      "/w/workspace-a/protocols#protocol-acp",
    );
    expect(protocolHelpPath("", "ai-sdk")).toBe(
      "/w/default/protocols#protocol-ai-sdk",
    );
    expect(protocolHelpPath("team space", "custom/protocol")).toBe(
      "/w/team%20space/protocols#protocol-custom%2Fprotocol",
    );
    expect(protocolHelpPath("team space", "managed", {
      agentId: "agent/one",
      environmentId: "env local",
    })).toBe(
      "/w/team%20space/protocols?agent=agent%2Fone&environment=env+local#protocol-managed",
    );
    expect(protocolHelpPath("workspace-a", "managed", {
      agentId: " ",
      environmentId: null,
    })).toBe("/w/workspace-a/protocols#protocol-managed");
  });

  /**
   * Overview journey decision table.
   * Causes: C1 an operator enters the Workspace overview; C2 NAV owns the
   * current route for every destination; C3 labels may be English or Chinese.
   * Effects: E1 the product intent is Connect→Build→Run→Observe→Integrate; E2 every step
   * reuses the exact NAV object and therefore its canonical route; E3 locale
   * changes copy only, never ownership or order. Rule R1: C1+C2+C3 ->
   * E1+E2+E3, with no parallel route registry or lifecycle state.
   */
  it("expresses the Agent proof journey over canonical navigation references", () => {
    expect(WORKSPACE_JOURNEY.map((step) => step.label)).toEqual(["Connect", "Build", "Run", "Observe", "Integrate"]);
    expect(WORKSPACE_JOURNEY.map((step) => step.destination)).toEqual([
      NAV.find((item) => item.key === "models"),
      NAV.find((item) => item.key === "agents"),
      NAV.find((item) => item.key === "sessions"),
      NAV.find((item) => item.key === "artifacts"),
      NAV.find((item) => item.key === "protocols"),
    ]);
    expect(navPath(WORKSPACE_JOURNEY[2].destination, "workspace-a")).toBe("/w/workspace-a/sessions");
    expect(navPath(WORKSPACE_JOURNEY[4].destination, "workspace-a")).toBe("/w/workspace-a/protocols");
  });

  /**
   * Hosted navigation cause/effect table.
   * R1 BYOK + Managed runtime + embedded IAM -> retain the full canonical IA.
   * R2 managed supply + split Control + remote IAM -> only overview, Agent,
   * model and settings surfaces, with Models relabelled for managed supply.
   * R3 Managed runtime + remote IAM -> retain runtime, omit Access.
   * R4 rolling-version skew without the surface projection -> fail closed to
   * the untagged authoring routes instead of throwing or guessing availability.
   * Constraint: all rules filter NAV; no second route registry is introduced.
   */
  it("projects model-supply posture without creating another route registry", () => {
    const local = visibleNavigation(capabilities(true, true, true));
    const hosted = visibleNavigation(capabilities(false, false, false));
    expect(local.find((item) => item.key === "models")?.label).toBe("Models & providers");
    expect(hosted.find((item) => item.key === "models")?.label).toBe("Models");
    expect(hosted.find((item) => item.key === "models")?.group).toBe("author");
    expect(local.map((item) => item.path)).toEqual(NAV.map((item) => item.path));
    expect(hosted.map((item) => item.key)).toEqual(["overview", "agents", "models", "settings"]);
    expect(local.some((item) => item.key === "credentials")).toBe(false);
    expect(hosted.some((item) => item.key === "credentials")).toBe(false);

    const runtimeWithoutEmbeddedIam = visibleNavigation(capabilities(true, true, false));
    expect(runtimeWithoutEmbeddedIam.some((item) => item.key === "sessions")).toBe(true);
    expect(runtimeWithoutEmbeddedIam.some((item) => item.key === "access")).toBe(false);

    const { surfaces: _surfaces, ...oldResponse } = capabilities(false, false, false);
    expect(visibleNavigation(oldResponse as ConfigCapabilitiesView).map((item) => item.key))
      .toEqual(["overview", "agents", "models", "settings"]);
  });

  /**
   * Workspace-label partitions: default, hosted personal, short authored, and
   * long opaque coordinates cause friendly, personal, unchanged, and bounded
   * effects respectively. These four rows cover every branch of workspaceLabel.
   */
  it("bounds opaque workspace coordinates without changing short authored names", () => {
    expect(workspaceLabel("default")).toBe("Default Workspace");
    expect(workspaceLabel("awaken:personal:acct_0123456789")).toBe("Personal Workspace");
    expect(workspaceLabel("workspace_local_126cc_18cbbe83d0355538")).toBe("Local Workspace");
    expect(workspaceLabel("product-design")).toBe("Product Design Workspace");
    expect(workspaceLabel("workspace_abcdefghijklmnopqrstuvwxyz_0123456789")).toBe(
      "Workspace",
    );
  });
});
